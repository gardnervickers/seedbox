//! One x86-64 Linux machine: a KVM VM with one vCPU, run only by the world
//! thread. Guest progress becomes virtual time through the PV clock (see
//! `pv.rs`): time moves only at exits, syscalls and guest clock reads.

use crate::boundary::{advance_time, hard_limit, io_charge, PendingIo};
use crate::devices::{rtc_read, Pic, Pit, Uart};
use crate::disk::Disk;
use crate::shadow::{Shadow, ShadowStats, VcpuState};
use crate::virtio_blk::VirtioBlk;
use crate::virtio_console::VirtioConsole;
use crate::virtio_net::{ReceiveOutcome, VirtioNet};
use crate::witness_log::WitnessLog;
use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use sha2::{Digest, Sha256};
use std::os::fd::AsRawFd;
use std::time::Instant;

mod checkpoint;
#[cfg(test)]
mod device_checkpoint_tests;
pub mod pv;
mod pv_plan;
pub use checkpoint::MachineSnapshot;
use pv::PvClock;

pub const TIME_PAGE: u64 = 0xd000_0000;
pub const NET_MMIO: u64 = 0xd000_1000;
pub const NET_IRQ: u8 = 5;
pub const CHANNEL_MMIO: u64 = 0xd1000000;
pub const CHANNEL_IRQ: u8 = 6;
pub const BLK_MMIO: u64 = 0xd000_2000;
pub const BLK_IRQ: u8 = 7;
const INITRD_ADDR: usize = 0x0800_0000;
/// Time page fields (u64 index), see kernel/seedbox.h.
const PAGE_SYSCALLS: usize = 3;
const PAGE_PIC_IMR: usize = 4;
const PAGE_NET_ISR: usize = 5;
pub const PAGE_FIELDS: usize = 6;
/// Fork entropy (ABI 3): written by the VMM only while the world is parked.
const PAGE_ENTROPY_GEN: usize = 9;
const PAGE_ENTROPY: usize = 10;
const PAGE_ENTROPY_FIELDS: usize = 4;
/// Guest ABI from which the kernel reseeds its CRNG from the page.
pub const ABI_PAGE_RESEED: u64 = 3;
/// PV clock fields, written by the VMM and outside `PAGE_FIELDS` (checkpoints
/// and digests skip them; the clock restores them itself).
const PAGE_PV_CLOCK: usize = 6;
const PAGE_BOUND: usize = 7;
const PAGE_SYSCALL_TARGET: usize = 8;
const PAGE_TIMER_DEADLINE: usize = 14;
const PAGE_CAPABILITIES: usize = 15;
// Rewritten at every plan, outside the checkpointed PAGE_FIELDS.
const PAGE_SYSCALL_NS: usize = 16;
const PAGE_SYSCALLS0: usize = 17;
const PAGE_PLAN_TIME: usize = 18;
/// Progress jitter: the bound and the seed of the stream the kernel draws
/// from at each progress call. Written at boot, carried across a reboot and
/// reseeded with the other streams at a fork.
const PAGE_PROGRESS_NS: usize = 19;
const PAGE_PROGRESS_JITTER_NS: usize = 20;
const PAGE_PROGRESS_JITTER_SEED: usize = 21;
pub const ABI_PAGE_TIMER: u64 = 4;
const CAP_PAGE_TIMER: u64 = 1;
const TIMER_DISARMED: u64 = u64::MAX;

/// The last adopted page store. Kept separately so expiration cannot rearm
/// an unchanged word, and legacy PIT writes can still replace a page arm.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
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
    assert!(PAGE_PROGRESS_NS == 19);
    assert!(PAGE_PROGRESS_JITTER_NS == 20);
    assert!(PAGE_PROGRESS_JITTER_SEED == 21);
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
    /// The guest parks at its launch barrier until the world releases it.
    pub barrier: bool,
    pub page_timer: bool,
}

/// Port of the launch barrier, see guest/barrier.h.
const BARRIER_PORT: u16 = 0xf8;
pub const ENTROPY_WORDS: usize = 8;

/// Everything about one machine that a world fork puts back: vCPU, RAM, the
/// shared page, device models and the VMM's per-machine scalars.
pub struct MemorySnapshot {
    ram: Mapping,
    page: Box<[u8; 4096]>,
    blk: Option<VirtioBlk>,
    state: SnapshotState,
    artifact: std::sync::OnceLock<checkpoint::Artifact>,
}

impl std::ops::Deref for MemorySnapshot {
    type Target = SnapshotState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotState {
    world: MachineState,
    pending_io: Option<PendingIo>,
    timer_rng: Option<crate::rng::Rng>,
    entropy: [u32; ENTROPY_WORDS],
    entropy_read: usize,
    cpu: VcpuState,
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
    log_decoder: seedbox_engine_core::observation::Decoder,
    net: Option<VirtioNet>,
    channel: Option<VirtioConsole>,
    control_dma_bytes: u64,
    control_tx_bytes: u64,
    pv: bool,
    syscall_ns: u64,
    throttle_permille: Option<u64>,
    #[serde(default)]
    pv_debt_ns: u64,
    #[serde(default)]
    cpu_speed_hz: u64,
    abi: u64,
    page_timer: PageTimer,
    pit_exact: bool,
    exit_req: bool,
    stats: Stats,
    #[serde(with = "seedbox_engine_core::checkpoint::sha256")]
    witness: Sha256,
    witness_log: WitnessLog,
    shadow_stats: ShadowStats,
}

use seedbox_engine_core::world::MachineState;
pub use seedbox_engine_core::world::{Reseed, State, Stop};

/// The time page's entropy fields: the seed's 32 bytes, little-endian.
fn page_entropy(entropy: &[u32; ENTROPY_WORDS]) -> [u64; PAGE_ENTROPY_FIELDS] {
    std::array::from_fn(|i| entropy[2 * i] as u64 | (entropy[2 * i + 1] as u64) << 32)
}

/// A raw pointer to memory its owner maps and unmaps. The owner is the only
/// user, so moving the owner to another thread moves the mapping with it.
/// Only owning fields use it, so any other non-`Send` field added to a
/// machine is still caught by the compiler.
#[derive(Clone, Copy)]
pub(crate) struct SendPtr<T>(pub(crate) *mut T);
// SAFETY: see above; the pointee is reached only through its owner.
unsafe impl<T> Send for SendPtr<T> {}

/// An anonymous private mapping, unmapped on drop.
struct Mapping {
    ptr: SendPtr<u8>,
    size: usize,
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.0.cast(), self.size) };
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub exits: u64,
    pub pio: u64,
    pub mmio: u64,
    pub halts: u64,
    pub irqs: u64,
    pub window_exits: u64,
    pub eintr: u64,
    pub run_ns: u128,
    pub yields: u64,
    pub yield_stops: u64,
    pub kvm_ns: [u128; 8],
    pub kvm_n: [u64; 8],
    /// Stalls reported: runs with no exit for long enough that the watchdog
    /// printed where the guest was.
    pub stalls: u64,
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
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Charged syscalls.
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
    _vm: VmFd,
    vcpu: VcpuFd,
    mem: SendPtr<u8>,
    mem_size: usize,
    page: SendPtr<u64>,
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
    pub observations: Option<std::sync::Arc<seedbox_engine_core::observation::Publisher>>,
    log_decoder: seedbox_engine_core::observation::Decoder,
    log_order: u64,
    management_framer: crate::control::ManagementFramer,
    pub net: Option<VirtioNet>,
    pub channel: Option<VirtioConsole>,
    pub blk: Option<VirtioBlk>,
    pub control_dma_bytes: u64,
    pub control_tx_bytes: u64,
    exit_req: bool,
    /// The guest uses the shared page for interrupt masks and status (ABI 2).
    pv: bool,
    /// Guest ABI version announced on the time page port (0 before then).
    abi: u64,
    page_timer: PageTimer,
    /// The pending timer deadline must be delivered at its exact time.
    pit_exact: bool,
    pub exact_timer_ppm: u64,
    pub tolerance: u64,
    pub timer_jitter: u64,
    /// PV clock: virtual time one syscall costs on this machine.
    pub syscall_ns: u64,
    /// A throttle fault's factor on `syscall_ns`, in thousandths. `None`
    /// until the first throttle, so unthrottled runs charge exactly as before.
    pub throttle_permille: Option<u64>,
    /// PV clock: virtual time the guest's counted syscalls already owe past
    /// the last hard stop, charged at the start of the next run.
    pub pv_debt_ns: u64,
    /// The machine's configured CPU speed, which a throttle is a fraction of.
    pub cpu_speed_hz: u64,
    /// Consecutive watchdog interruptions since the last natural exit.
    stall_ticks: u64,
    pub timer_rng: Option<crate::rng::Rng>,
    /// Shadow RAM with a dirty log: a fork restore copies back only the
    /// pages written since the fork point.
    pub shadow: Box<Shadow>,
    /// Whether slot 0 carries KVM's dirty log, which only forks need.
    dirty_log: bool,
    /// The initial guest uses the launch-barrier boot protocol.
    barrier: bool,
    /// Per-seed words the guest reads after its release.
    entropy: [u32; ENTROPY_WORDS],
    entropy_read: usize,
    page_timer_requested: bool,
    /// The MSR list a fork snapshot saves.
    msrs: Msrs,
    pub stats: Stats,
    pub witness: Sha256,
    /// Periodic witness samples; diagnostic only, never hashed.
    pub witness_log: WitnessLog,
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

/// Count consecutive watchdog interruptions of a run with no exit and, at
/// each doubling past the first few, say where the guest is. A diagnostic
/// only: it changes nothing the guest or the witness sees.
#[allow(clippy::too_many_arguments)]
fn note_watchdog(
    stall_ticks: &mut u64,
    stats: &mut Stats,
    name: &str,
    time: u64,
    rip: u64,
    interrupted: bool,
    period: std::time::Duration,
) {
    if !interrupted {
        *stall_ticks = 0;
        return;
    }
    *stall_ticks += 1;
    if *stall_ticks >= 32 && stall_ticks.is_power_of_two() {
        stats.stalls += 1;
        eprintln!(
            "{name}: no guest exit for {} ms of host CPU time at rip {rip:#x} (virtual time {time} ns)",
            (period * *stall_ticks as u32).as_millis(),
        );
    }
}

impl Machine {
    pub fn new(kvm: &Kvm, name: &str, spec: &BootSpec) -> Result<Self, String> {
        if !kvm.check_extension(kvm_ioctls::Cap::ImmediateExit) {
            return Err("KVM immediate-exit capability required for I/O completion".into());
        }
        let mem_size = spec.mem;
        let mem = map(mem_size)?;
        // Fewer, larger host faults while the guest touches fresh RAM. The
        // guest sees the same zeroed memory; only host paging changes.
        unsafe { libc::madvise(mem.cast(), mem_size, libc::MADV_HUGEPAGE) };
        let page = map(4096)?;
        // Tell the kernel to run its PV clock; it answers 2 on the ABI port.
        unsafe { std::ptr::write_volatile(page.cast::<u64>().add(PAGE_PV_CLOCK), 1) };
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
                // The dirty log starts at the first fork point. Until then
                // KVM can map guest RAM with huge pages.
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
        let msrs = crate::shadow::msr_list(kvm, &vcpu)?;
        let mut shadow = Shadow::new(mem_size)?;
        // Everything loaded so far was written by the VMM.
        shadow.mark(0, 0x100000);
        shadow.mark(0x100000, k.len() - setup);
        shadow.mark(INITRD_ADDR, spec.initrd.len());
        Ok(Self {
            world: MachineState::new(name.to_string()),
            _vm: vm,
            vcpu,
            mem: SendPtr(mem),
            mem_size,
            page: SendPtr(page.cast()),
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
            log_decoder: seedbox_engine_core::observation::Decoder::default(),
            observations: None,
            net,
            channel: spec.control_channel.then(VirtioConsole::new),
            blk: spec.disk.clone().map(VirtioBlk::new),
            control_dma_bytes: 0,
            control_tx_bytes: 0,
            exit_req: false,
            pv: false,
            abi: 0,
            page_timer: PageTimer::new(),
            pit_exact: false,
            exact_timer_ppm: 0,
            tolerance: 0,
            timer_jitter: 0,
            syscall_ns: pv::SYSCALL_NS,
            throttle_permille: None,
            pv_debt_ns: 0,
            cpu_speed_hz: 0,
            stall_ticks: 0,
            timer_rng: None,
            shadow: Box::new(shadow),
            dirty_log: false,
            barrier: spec.barrier,
            entropy: [0; ENTROPY_WORDS],
            entropy_read: 0,
            page_timer_requested: spec.page_timer,
            msrs,
            stats: Stats::default(),
            witness: Sha256::new(),
            witness_log: WitnessLog::default(),
            trace: None,
        })
    }

    /// Guest ABI announced on the time page port, 0 before the kernel has.
    pub fn abi(&self) -> u64 {
        self.abi
    }

    pub fn memory(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.mem.0, self.mem_size) }
    }

    fn record(&mut self, kind: u8, extra: u64) {
        if let Some(t) = self.trace.as_mut() {
            use std::io::Write;
            let rip = self.vcpu.get_regs().map(|r| r.rip).unwrap_or(0);
            let _ = writeln!(
                t,
                "{} {} {:x} {:x}",
                kind as char, self.world.time, extra, rip
            );
        }
        self.witness.update([kind]);
        self.witness.update(self.world.time.to_le_bytes());
        self.witness.update(extra.to_le_bytes());
        self.witness_log.record(self.world.time, &self.witness);
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

    /// Publish what a progress call costs: its nominal charge, the bound on
    /// its seeded variation and the first seed of the stream the guest kernel
    /// draws that variation from. All are page words outside the checkpointed
    /// prefix, so a snapshot carries them like the plan.
    pub fn set_progress(&mut self, progress_ns: u64, jitter_ns: u64, seed: u64) {
        self.page_set(PAGE_PROGRESS_NS, progress_ns);
        self.page_set(PAGE_PROGRESS_JITTER_NS, jitter_ns);
        self.page_set(PAGE_PROGRESS_JITTER_SEED, seed);
    }

    /// Give the progress jitter stream a new seed; the kernel restarts the
    /// stream from it at its next progress call.
    pub fn seed_progress_jitter(&mut self, seed: u64) {
        self.page_set(PAGE_PROGRESS_JITTER_SEED, seed);
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
        self.record(b'N', self.world.incarnation() as u64);
        Ok(())
    }

    /// Carry what outlives a crash from the previous incarnation into this
    /// one, which is built but not booted: the console and management logs
    /// and their sinks, the trace, and the event witness, so the final
    /// witness covers every incarnation. Nothing else crosses: RAM, device
    /// queues and KVM's I/O state die with the VM.
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
            new.discard_output = old.discard_output;
            new.transmitted = old.transmitted;
            new.chunks = std::mem::take(&mut old.chunks);
        }
        self.log_order = old.log_order;
        self.trace = old.trace.take();
        self.witness_log = std::mem::take(&mut old.witness_log);
        // The same machine: under the PV clock it keeps its speed, or members
        // restarted together would run in lockstep again.
        self.syscall_ns = old.syscall_ns;
        self.cpu_speed_hz = old.cpu_speed_hz;
        // Flushed data survives; the disk policy chooses unflushed survivors.
        let lost = if let (Some(new), Some(blk)) = (self.blk.as_mut(), old.blk.as_ref()) {
            let (disk, lost) =
                blk.disk()
                    .crashed_branch(seed, identity, &old.world.name, old.world.incarnation());
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
        // As at the launch barrier: finish any exit KVM still owes, so a cold
        // run and a restored fork resume from identical vCPU state.
        self.complete_io()?;
        self.world.commit_park();
        self.record(b'P', 0);
        Ok(())
    }

    /// Capture at a committed scheduler boundary, including unpaid I/O cost.
    pub fn snapshot(&mut self) -> Result<MachineSnapshot, String> {
        if !self.world.fault_effects.is_empty()
            || !self.world.control_requests.is_empty()
            || self.world.committed_console.is_some()
            || !self.world.committed_output.is_empty()
        {
            return Err(format!("{}: not at a fork point", self.world.name));
        }
        self.complete_io()?;
        let mut cpu = VcpuState::new(self.msrs.clone());
        cpu.save(&self.vcpu, "fork")?;
        let ram = Mapping {
            ptr: SendPtr(map(self.mem_size)?),
            size: self.mem_size,
        };
        unsafe { std::ptr::copy_nonoverlapping(self.mem.0, ram.ptr.0, self.mem_size) };
        self.hold_fork_point()?;
        let mut page = Box::new([0u8; 4096]);
        unsafe { std::ptr::copy_nonoverlapping(self.page.0 as *const u8, page.as_mut_ptr(), 4096) };
        Ok(MachineSnapshot::Memory(Box::new(MemorySnapshot {
            ram,
            page,
            blk: self.blk.clone(),
            artifact: Default::default(),
            state: SnapshotState {
                cpu,
                world: self.world.clone(),
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
                log_decoder: self.log_decoder.clone(),
                net: self.net.clone(),
                channel: self.channel.clone(),
                control_dma_bytes: self.control_dma_bytes,
                control_tx_bytes: self.control_tx_bytes,
                pv: self.pv,
                syscall_ns: self.syscall_ns,
                throttle_permille: self.throttle_permille,
                pv_debt_ns: self.pv_debt_ns,
                cpu_speed_hz: self.cpu_speed_hz,
                abi: self.abi,
                page_timer: self.page_timer,
                pit_exact: self.pit_exact,
                exit_req: self.exit_req,
                stats: self.stats.clone(),
                witness: self.witness.clone(),
                witness_log: self.witness_log.clone(),
                shadow_stats: self.shadow.stats.clone(),
            },
        })))
    }

    /// Hold a fork point in the shadow, starting KVM's dirty log first if
    /// this machine has not forked before. Nothing was logged until then, so
    /// every page is copied once.
    fn hold_fork_point(&mut self) -> Result<(), String> {
        if !self.dirty_log {
            unsafe {
                self._vm
                    .set_user_memory_region(kvm_userspace_memory_region {
                        slot: 0,
                        flags: KVM_MEM_LOG_DIRTY_PAGES,
                        guest_phys_addr: 0,
                        memory_size: self.mem_size as u64,
                        userspace_addr: self.mem.0 as u64,
                    })
                    .map_err(|e| format!("ram slot dirty log: {e}"))?;
            }
            self.dirty_log = true;
            self.shadow.mark(0, self.mem_size);
        }
        self.shadow.fork_point(&self._vm, self.mem.0)
    }

    /// Put the machine back to a snapshot taken by `snapshot`, from wherever
    /// the last run left it. Returns the number of RAM pages copied back.
    pub fn restore(&mut self, snapshot: &MachineSnapshot) -> Result<u64, String> {
        match snapshot {
            MachineSnapshot::Memory(saved) => self.restore_memory(saved),
            MachineSnapshot::File(path) => {
                let saved = checkpoint::read(self, path)?;
                self.restore_memory(&saved)
            }
        }
    }

    fn restore_memory(&mut self, s: &MemorySnapshot) -> Result<u64, String> {
        // The last exit may still owe KVM its completion (an exit port write,
        // say); complete it so the vCPU accepts new state. Its effect is
        // overwritten below, and a vCPU that cannot run any more (shutdown)
        // is overwritten just the same.
        let _ = self.complete_io();
        self.stall_ticks = 0;
        let pages = if !self.shadow.holds_fork_point() {
            // A machine rebuilt for this restore (the run rebooted it) has
            // held no fork point yet: copy everything, then hold one here.
            unsafe { std::ptr::copy_nonoverlapping(s.ram.ptr.0, self.mem.0, self.mem_size) };
            self.shadow.mark(0, self.mem_size);
            self.hold_fork_point()?;
            (self.mem_size / 4096) as u64
        } else {
            self.shadow
                .return_to_fork(&self._vm, self.mem.0, s.ram.ptr.0)?
        };
        // Each seed reports its own shadow work on top of the boot's.
        self.shadow.stats = s.shadow_stats.clone();
        unsafe { std::ptr::copy_nonoverlapping(s.page.as_ptr(), self.page.0 as *mut u8, 4096) };
        let (regs, sregs) = s.cpu.restore(&self.vcpu, "fork restore")?;
        // Refresh KVM's exit outputs (interrupt flag, injection readiness and
        // the synchronized registers) from the restored state, exactly as the
        // completion at the barrier did when the snapshot was taken.
        self.complete_io()?;
        let run = self.vcpu.get_kvm_run();
        run.request_interrupt_window = 0;
        run.s.regs.regs = regs;
        run.s.regs.sregs = sregs;
        self.world = s.world.clone();
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
        self.log_decoder = s.log_decoder.clone();
        self.net = s.net.clone();
        self.channel = s.channel.clone();
        self.blk = s.blk.clone();
        self.control_dma_bytes = s.control_dma_bytes;
        self.control_tx_bytes = s.control_tx_bytes;
        self.pv = s.pv;
        self.syscall_ns = s.syscall_ns;
        self.throttle_permille = s.throttle_permille;
        self.pv_debt_ns = s.pv_debt_ns;
        if s.cpu_speed_hz != 0 {
            self.cpu_speed_hz = s.cpu_speed_hz;
        }
        self.abi = s.abi;
        self.page_timer = s.page_timer;
        self.pit_exact = s.pit_exact;
        self.exit_req = s.exit_req;
        self.entropy = s.entropy;
        self.entropy_read = s.entropy_read;
        self.stats = s.stats.clone();
        self.witness = s.witness.clone();
        self.witness_log = s.witness_log.clone();
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
            self.pic.raise(3);
        }
    }

    /// Strict runtime delivery requires all bytes DMA into posted guest buffers.
    pub fn deliver_control(&mut self, bytes: &[u8]) -> Result<(), String> {
        let memory = unsafe { std::slice::from_raw_parts_mut(self.mem.0, self.mem_size) };
        let channel = self
            .channel
            .as_mut()
            .ok_or("runtime control channel absent")?;
        let received = channel.receive(memory, bytes)?;
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
            if uart.discard_output {
                continue;
            }
            uart.chunks.capture(
                uart.out.len() as u64,
                self.world.time,
                self.world.incarnation(),
                &mut self.log_order,
            );
        }
    }
    fn channel_output(&mut self, bytes: &[u8]) -> Result<bool, String> {
        let output_time = self.world.time;
        self.log_decoder.commit(
            &mut self.world,
            self.observations.as_deref(),
            output_time,
            bytes,
        );
        if !self.com2.discard_output
            && self.com2.output_limit.is_some_and(|limit| {
                self.com2
                    .out
                    .len()
                    .checked_add(bytes.len())
                    .is_none_or(|n| n > limit)
            })
        {
            return Err("guest control output limit exceeded".into());
        }
        if let Some(file) = self.com2.output_file.as_mut() {
            use std::io::Write;
            file.write_all(bytes)
                .map_err(|e| format!("control log: {e}"))?;
        }
        self.com2.transmitted = self.com2.transmitted.saturating_add(bytes.len() as u64);
        if !self.com2.discard_output {
            self.com2.out.extend_from_slice(bytes);
        }
        self.control_tx_bytes += bytes.len() as u64;
        self.capture_output();
        let frames = self.management_framer.receive(bytes)?;
        let stop = !frames.is_empty() || !self.world.committed_output.is_empty();
        self.world
            .control_requests
            .extend(frames.into_iter().map(|f| (self.world.time, f)));
        Ok(stop)
    }

    pub fn deliver_frame(&mut self, frame: &[u8]) -> Result<ReceiveOutcome, String> {
        let (mem, size) = (self.mem.0, self.mem_size);
        let m = unsafe { std::slice::from_raw_parts_mut(mem, size) };
        let outcome = self.net.as_mut().map(|n| n.receive(m, frame)).transpose();
        self.take_dma();
        let outcome = outcome?.unwrap_or_default();
        if outcome.irq {
            self.raise_net();
        }
        Ok(outcome)
    }

    fn poll_net(&mut self, frames: &mut Vec<Vec<u8>>, boundary: bool) -> Result<bool, String> {
        let m = unsafe { std::slice::from_raw_parts_mut(self.mem.0, self.mem_size) };
        let Some(n) = self.net.as_mut() else {
            return Ok(false);
        };
        let out = n.poll_at(m, self.world.time, boundary);
        self.take_dma();
        let out = out?;
        if out.irq {
            self.raise_net();
        }
        let sent = !out.frames.is_empty();
        frames.extend(out.frames);
        Ok(sent)
    }

    /// Syscalls the guest has entered (counted in the time page).
    /// Move virtual time forward to `t` (never back) for `cause`.
    fn advance_to(&mut self, t: u64, cause: Cause) {
        if t > self.world.time {
            self.stats.time_by[cause as usize] += t - self.world.time;
            self.world.time = t;
        }
    }

    pub fn syscalls(&self) -> u64 {
        self.page_get(PAGE_SYSCALLS)
    }

    fn page_get(&self, i: usize) -> u64 {
        unsafe { std::ptr::read_volatile(self.page.0.add(i)) }
    }

    fn page_set(&mut self, i: usize, v: u64) {
        unsafe { std::ptr::write_volatile(self.page.0.add(i), v) }
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

    /// Account device writes to guest RAM for the shadow's dirty log.
    fn take_dma(&mut self) {
        if let Some(n) = self.net.as_mut() {
            for (a, l) in n.dirty.drain(..) {
                self.shadow.mark(a, l);
            }
        }
        if let Some(channel) = self.channel.as_mut() {
            for (address, length) in channel.dirty.drain(..) {
                self.shadow.mark(address, length);
            }
        }
        if let Some(blk) = self.blk.as_mut() {
            for (a, l) in blk.dirty.drain(..) {
                self.shadow.mark(a, l);
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
            Err(e) => Err(format!("{}: completing I/O: {e}", self.world.name)),
            Ok(e) => Err(format!(
                "{}: exit while completing I/O: {e}",
                self.world.name
            )),
        }
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
        hex::encode(h.finalize())
    }

    fn note_skew(&mut self, site: Site) {
        let last = self.page_get(1);
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
    // call from the clock's prepare or a watchdog landing.
    fn sync_page_timer(&mut self) -> Result<(), String> {
        if !self.page_timer.enabled {
            return Ok(());
        }
        if self.page_timer.pickup(
            self.page_get(PAGE_TIMER_DEADLINE),
            &mut self.pit,
            self.world.time,
        )? {
            self.pic.raise(0);
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
            if self.world.time == limit {
                return Ok(false);
            }
            let t = pending.advance(self.world.time, limit, self.pit.deadline)?;
            self.advance_to(t, Cause::Io);
            self.expire_timers()?;
        }
        Ok(true)
    }

    /// Pay counted CPU work before resuming the guest. As with owed I/O,
    /// timer expiry may make an IRQ pending, but must not execute the vCPU.
    fn drain_progress(&mut self, limit: u64) -> Result<(), String> {
        while self.pv_debt_ns != 0 && self.world.time < limit {
            self.expire_timers()?;
            let end = limit.min(self.pit.deadline.unwrap_or(u64::MAX));
            let (charged, owed) = pv_plan::charge(self.pv_debt_ns, 0, end - self.world.time);
            self.pv_debt_ns = owed;
            self.advance_to(self.world.time + charged, Cause::Progress);
            self.expire_timers()?;
        }
        Ok(())
    }

    fn checked_page_time(&self, limit: u64) -> Result<u64, String> {
        let last = unsafe { std::ptr::read_volatile(self.page.0.add(1)) };
        advance_time(self.world.time, last.saturating_sub(self.world.time), limit)
    }

    fn sync_from_page(&mut self, limit: u64) -> Result<(), String> {
        let t = self.checked_page_time(limit)?;
        self.advance_to(t, Cause::ClockReads);
        self.sync_pic();
        Ok(())
    }

    fn publish(&mut self) {
        unsafe { std::ptr::write_volatile(self.page.0, self.world.time) };
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
        clock: &mut PvClock,
        limit: u64,
        early: u64,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        let started = Instant::now();
        let r = self.run_inner(clock, limit, early, frames);
        self.capture_output();
        self.com1.chunks.finish();
        self.com2.chunks.finish();
        self.stats.run_ns += started.elapsed().as_nanos();
        r
    }

    fn run_inner(
        &mut self,
        clock: &mut PvClock,
        limit: u64,
        early: u64,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        // Invariant: entered at a deterministic point. Guest-visible state (time
        // page, interrupts, timer expiry) changes only at deterministic points:
        // natural exits, never at watchdog landings.
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
            if self.pending_io.is_some() && !self.drain_io(limit)? {
                // Charged I/O can split at an exact slice boundary too.
                if self.poll_net(frames, true)? {
                    self.note_skew(Site::Sent);
                    return Ok(Stop::Sent);
                }
                return Ok(Stop::Reached);
            }
            if legal {
                // Guest clock reads can advance time between exits. Check
                // before expiring timers or accepting the slice boundary.
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
                self.drain_progress(slice_end)?;
                // Also runs on slice entry after world RX delivery. Never poll
                // at watchdog landings.
                if self.poll_net(frames, self.world.time >= slice_end)? {
                    self.note_skew(Site::Sent);
                    return Ok(Stop::Sent);
                }
                if self.world.time >= slice_end {
                    return Ok(Stop::Reached);
                }
                self.publish();
            }
            // Stop at the first natural exit at or after `soft` and below
            // `hard`: the guest exits by itself when its clock would pass the
            // bound published from `soft`, or at the syscall that reaches it.
            // Two rules combine: the slice ends in [slice_end, limit], and a
            // timer due at `pit` fires in [pit, pit + tolerance] (exactly at
            // `pit` if so drawn). Neither passes `limit`, the world's causal
            // boundary.
            let pit = self.pit.deadline.unwrap_or(u64::MAX);
            let soft = slice_end.min(pit);
            let hard = hard_limit(limit, self.pit.deadline, self.pit_exact, self.tolerance);
            let remaining = hard - self.world.time;
            // Interrupt injection depends only on the distance to the stop,
            // so it is deterministic.
            if legal {
                if remaining > 0 {
                    self.inject();
                } else {
                    self.vcpu.get_kvm_run().request_interrupt_window = 0;
                }
            }
            if self.exit_req {
                unsafe { std::ptr::write_volatile(self.page.0.add(2), 0) };
                self.exit_req = false;
            }
            let syscalls0 = self.page_get(PAGE_SYSCALLS);
            clock.prepare(self, remaining, soft, legal)?;
            self.vcpu.get_kvm_run().kvm_valid_regs =
                (KVM_SYNC_X86_REGS | KVM_SYNC_X86_SREGS) as u64;
            clock.start(syscalls0);
            let t_run = Instant::now();
            let run_ptr = self.vcpu.get_kvm_run() as *mut kvm_run;
            let exit = self.vcpu.run();
            let run_ns = t_run.elapsed().as_nanos();
            let kind = match &exit {
                Err(_) => 0,
                Ok(VcpuExit::IoOut(..)) | Ok(VcpuExit::IoIn(..)) => 2,
                Ok(VcpuExit::MmioRead(..)) | Ok(VcpuExit::MmioWrite(..)) => 3,
                Ok(VcpuExit::Hlt) => 4,
                Ok(VcpuExit::IrqWindowOpen) => 5,
                _ => 6,
            };
            self.stats.kvm_ns[kind] += run_ns;
            self.stats.kvm_n[kind] += 1;
            let syscalls1 = unsafe { std::ptr::read_volatile(self.page.0.add(PAGE_SYSCALLS)) };
            let delta = clock.read(&mut self.pv_debt_ns, syscalls1);
            let interrupted = matches!(&exit, Err(e) if e.errno() == libc::EINTR);
            let run_syscalls = syscalls1.wrapping_sub(syscalls0);
            // A watchdog interruption is not a deterministic point: the run
            // resumes without publishing, injecting or adopting anything.
            legal = !interrupted;
            note_watchdog(
                &mut self.stall_ticks,
                &mut self.stats,
                &self.world.name,
                self.world.time,
                unsafe { (*run_ptr).s.regs.regs.rip },
                interrupted,
                clock.period(),
            );
            self.stats.time_by[Cause::Progress as usize] += delta;
            self.world.time += delta;
            if legal {
                // Sent/Hlt/Exited can return before the next loop synchronizes
                // the page. Validate guest clock progress before exit effects.
                // The exit still borrows vcpu: use only the disjoint fields.
                let last = unsafe { std::ptr::read_volatile(self.page.0.add(1)) };
                let exit_time =
                    advance_time(self.world.time, last.saturating_sub(self.world.time), hard)?;
                // In particular HLT must adopt at its full exit time, not
                // at a watchdog landing's partial syscall-only time. A
                // stop that left the guest's clock ahead of ours could be
                // given a next limit below time the guest already read;
                // a timer armed here must count from that clock too.
                self.stats.time_by[Cause::ClockReads as usize] += exit_time - self.world.time;
                self.world.time = exit_time;
                if self.page_timer.enabled {
                    let word =
                        unsafe { std::ptr::read_volatile(self.page.0.add(PAGE_TIMER_DEADLINE)) };
                    if self
                        .page_timer
                        .pickup(word, &mut self.pit, self.world.time)?
                    {
                        self.pic.raise(0);
                    }
                    timer_latency(
                        &mut self.pit,
                        &mut self.pit_exact,
                        self.timer_rng.as_mut(),
                        self.timer_jitter,
                        self.exact_timer_ppm,
                    );
                    if self.pit.expire_through(self.world.time)? > 0 {
                        self.pic.raise(0);
                    }
                }
            }
            self.stats.exits += 1;
            let now = self.world.time;
            match exit {
                Err(e) if e.errno() == libc::EINTR => {
                    self.stats.eintr += 1;
                }
                Ok(VcpuExit::IoOut(0x52c, data)) => {
                    // Requested yield: a stop if we reached the soft target,
                    // otherwise transparent (nothing observable happens).
                    // 1 means the guest's clock would pass the bound we
                    // published (`soft`); it waits for time to move, so move
                    // it there.
                    let v = data[0];
                    if v == 1 {
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
                    let after_io = self.world.time;
                    let mut control_stop = false;
                    let console_before = self.com1.transmitted;
                    match port {
                        0x3f8..=0x3ff => {
                            if port == 0x3f8 && v == b'\n' && crate::console_trace_enabled() {
                                let line_start = self
                                    .com1
                                    .out
                                    .iter()
                                    .rposition(|&b| b == b'\n')
                                    .map(|p| p + 1)
                                    .unwrap_or(0);
                                crate::emit_console_trace(
                                    self.world.time,
                                    &self.com1.out[line_start..],
                                );
                            }
                            self.com1.write(port - 0x3f8, v)?
                        }
                        0x2f8..=0x2ff => {
                            if self.channel.is_some() {
                                self.com2.write(port - 0x2f8, v)?;
                            } else if let Some(line) = crate::management_uart::write(
                                &mut self.management_framer,
                                &mut self.com2,
                                port - 0x2f8,
                                v,
                            )? {
                                self.world.control_requests.push((after_io, line));
                                control_stop = true;
                            }
                        }
                        0xf6 | 0xf7 => {
                            let id = u32::from_le_bytes(
                                data.try_into()
                                    .map_err(|_| "fault effect must be a 32-bit I/O write")?,
                            );
                            self.world.fault_effects.push((id, after_io, port == 0xf6));
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
                                return Err(format!(
                                    "{}: checkpoint entropy requires guest ABI 3",
                                    self.world.name
                                ));
                            }
                            if self.page_get(PAGE_PV_CLOCK) != 2 {
                                return Err(format!(
                                    "{}: the guest kernel lacks PV clock support; rebuild it from kernel/",
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
                        0x40..=0x43 | 0x61 | 0x530 => {
                            let hi = data.get(1).copied().unwrap_or(0);
                            self.stats.timer_writes += 1;
                            self.note_skew(Site::Timer);
                            if let Some(d) = self.pit.deadline.filter(|&d| d <= now) {
                                // The run that ends here can pass the deadline
                                // (syscall charges). Rewriting the PIT would
                                // drop that tick.
                                let s = &mut self.stats;
                                s.timer_overdue += 1;
                                s.timer_overdue_max_ns = s.timer_overdue_max_ns.max(now - d);
                                s.timer_write_syscalls_max =
                                    s.timer_write_syscalls_max.max(run_syscalls);
                                // Deliver the due tick before the write.
                                self.expire_timers()?;
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
                            timer_latency(
                                &mut self.pit,
                                &mut self.pit_exact,
                                self.timer_rng.as_mut(),
                                self.timer_jitter,
                                self.exact_timer_ppm,
                            );
                        }
                        0x70 => self.rtc_index = v & 0x7f,
                        0xe9 => self.com1.transmit(v)?,
                        0xf4 => {
                            self.guest_exit_code = Some(v);
                            self.world.state = State::Exited(v);
                            self.record(b'X', v as u64);
                            return Ok(Stop::Exited);
                        }
                        BARRIER_PORT if self.barrier => {
                            if self.world.is_parked() || self.entropy_read != 0 {
                                return Err(format!(
                                    "{}: launch barrier reached twice",
                                    self.world.name
                                ));
                            }
                            self.world.commit_park();
                            self.record(b'B', 0);
                            // Finish the port write now, so the machine is
                            // parked in a state a fork can save and restore.
                            self.complete_io()?;
                            return Ok(Stop::Barrier);
                        }
                        0xf5 => {
                            self.guest_init_error = Some(v);
                            self.world.state = State::Exited(v);
                            self.record(b'E', v as u64);
                            return Ok(Stop::Exited);
                        }
                        _ => {}
                    }
                    self.capture_output();
                    if self.world.observe_console && self.com1.transmitted > console_before {
                        // Console ports have zero charge; the sink and pending KVM
                        // response are committed once at this deterministic exit.
                        self.world.committed_console = Some((after_io, v));
                        control_stop = true;
                    }
                    self.record(b'O', (port as u64) << 8 | v as u64);
                    self.serial_edges();
                    self.pending_io = pending;
                    if control_stop {
                        return Ok(Stop::Control);
                    }
                }
                Ok(VcpuExit::IoIn(port, data)) => {
                    self.stats.pio += 1;
                    let charge = io_charge(port, false);
                    advance_time(self.world.time, charge, u64::MAX)?;
                    let descriptor = unsafe { (*run_ptr).__bindgen_anon_1.io };
                    match port {
                        0x520 => {
                            let t = self.world.time.max(advance_time(self.tsc_latch, 1, hard)?);
                            self.tsc_latch = t;
                            self.stats.time_by[Cause::ClockReads as usize] += t - self.world.time;
                            self.world.time = t;
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
                        let memory =
                            unsafe { std::slice::from_raw_parts_mut(self.mem.0, self.mem_size) };
                        if let Some(channel) = self.channel.as_mut() {
                            let output = channel.write(memory, addr - CHANNEL_MMIO, v)?;
                            self.take_dma();
                            if output.irq {
                                self.pic.raise(CHANNEL_IRQ);
                            }
                            if self.channel_output(&output.bytes)? {
                                return Ok(Stop::Control);
                            }
                        }
                    }
                    if (BLK_MMIO..BLK_MMIO + 0x1000).contains(&addr) {
                        let m =
                            unsafe { std::slice::from_raw_parts_mut(self.mem.0, self.mem_size) };
                        if let Some(b) = self.blk.as_mut() {
                            let done = b.write(m, addr - BLK_MMIO, v)?;
                            self.take_dma();
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
                        let (mem, size) = (self.mem.0, self.mem_size);
                        let m = unsafe { std::slice::from_raw_parts_mut(mem, size) };
                        if let Some(n) = self.net.as_mut() {
                            let out = n.write_at(
                                m,
                                addr - NET_MMIO,
                                v,
                                self.world
                                    .time
                                    .max(unsafe { std::ptr::read_volatile(self.page.0.add(1)) }),
                            );
                            self.take_dma();
                            let out = out?;
                            if out.irq {
                                self.raise_net();
                            }
                            if !out.frames.is_empty() {
                                frames.extend(out.frames);
                                self.note_skew(Site::Sent);
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
                    return Ok(Stop::Exited);
                }
                Ok(other) => return Err(format!("{}: unexpected exit {other:?}", self.world.name)),
                Err(e) => return Err(format!("{}: KVM_RUN: {e}", self.world.name)),
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
        let hex = |h: Sha256| hex::encode(h.finalize());
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
        let mut xsave = self.vcpu.get_xsave().unwrap();
        crate::xsave::canonicalize_ymm(&mut xsave.region);
        for w in xsave.region {
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
            parts.push(("disk", b.disk().digest()));
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
                    barrier: false,
                    page_timer: false,
                };
                let mut m = Machine::new(&kvm, "net-poll", &spec).unwrap();
                let mut r = m.vcpu.get_regs().unwrap();
                r.rflags = 0x202;
                m.vcpu.set_regs(&r).unwrap();
                let memory = unsafe { std::slice::from_raw_parts_mut(m.mem.0, m.mem_size) };
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
                    .run_until(
                        &mut PvClock::new(std::time::Duration::from_millis(20)).unwrap(),
                        limit,
                        early,
                        &mut frames,
                    )
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
                barrier: false,
                page_timer: false,
            };
            let mut m = Machine::new(&kvm, "pv-stop", &spec).unwrap();
            let mut frames = Vec::new();
            let mut stop = m
                .run_until(
                    &mut PvClock::new(std::time::Duration::from_millis(20)).unwrap(),
                    10_000_000,
                    0,
                    &mut frames,
                )
                .unwrap();
            while stop == Stop::Reached {
                stop = m
                    .run_until(
                        &mut PvClock::new(std::time::Duration::from_millis(20)).unwrap(),
                        10_000_000,
                        0,
                        &mut frames,
                    )
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
}

#[cfg(test)]
mod io_boundary_tests {
    use super::*;

    fn probe(kvm: &Kvm, port: u16, write: bool) -> Machine {
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
            barrier: false,
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
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM on a Linux host")]
    fn page_capture_has_documented_format_and_preserves_ram_witness_state() {
        let kvm = Kvm::new().unwrap();
        let mut m = probe(&kvm, 0x530, true);
        let before = (m.exit_witness(), m.state_digest(), m.memory().to_vec());
        let pages = m.page_hashes();
        assert_eq!(pages.len(), m.mem_size / 4096 * 8);
        for (pfn, page) in before.2.chunks(4096).enumerate() {
            let hash = u64::from_le_bytes(Sha256::digest(page)[..8].try_into().unwrap());
            assert_eq!(&pages[pfn * 8..pfn * 8 + 8], &hash.to_le_bytes());
        }
        assert_eq!(
            (m.exit_witness(), m.state_digest(), m.memory().to_vec()),
            before
        );
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
        assert_eq!(fields["progress_ns"], (PAGE_PROGRESS_NS, 1));
        assert_eq!(fields["progress_jitter_ns"], (PAGE_PROGRESS_JITTER_NS, 1));
        assert_eq!(
            fields["progress_jitter_seed"],
            (PAGE_PROGRESS_JITTER_SEED, 1)
        );
        assert_eq!(index, 22);
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
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn page_timer_negotiation_and_sync_use_the_separate_page_word() {
        let kvm = Kvm::new().unwrap();
        let mut m = probe_machine(&kvm, false);
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
        assert_eq!(m.next_deadline(), 10000);
        m.page_set(PAGE_TIMER_DEADLINE, TIMER_DISARMED);
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
            let mut m = probe_machine(&kvm, false);
            assert_eq!(m.page_get(PAGE_PV_CLOCK), 1, "fresh boot, ABI {abi}");
            assert!((7..14).all(|i| m.page_get(i) == 0), "fresh boot, ABI {abi}");
            for i in 6..14 {
                m.page_set(i, 100 + i as u64);
            }
            m.abi = abi;
            m.park().unwrap();
            let mut witness = m.witness.clone();
            witness.update([b'R']);
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
                hex::encode(witness.finalize()),
                "ABI {abi}"
            );
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn a_time_fork_writes_only_entropy_and_a_barrier_writes_none() {
        let kvm = Kvm::new().unwrap();
        for barrier in [false, true] {
            let mut m = probe_machine(&kvm, barrier);
            assert_eq!(m.page_get(PAGE_PV_CLOCK), 1, "fresh boot");
            assert!((7..14).all(|i| m.page_get(i) == 0), "fresh boot");
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

    fn probe_machine(kvm: &Kvm, barrier: bool) -> Machine {
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
            barrier,
            page_timer: false,
        };
        Machine::new(kvm, "fork-probe", &spec).unwrap()
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn runtime_checkpoint_restores_pv_fields_and_seeded_syscall_cost() {
        let kvm = Kvm::new().unwrap();
        let mut m = probe_machine(&kvm, false);
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
        let directory = tempfile::tempdir().unwrap();
        saved
            .persist(&directory.path().join("machine.snapshot"))
            .unwrap();
        let reference = serde_json::to_vec(&saved).unwrap();
        drop(saved);
        drop(m);
        let saved: MachineSnapshot = serde_json::from_slice(&reference).unwrap();
        let mut m = probe_machine(&kvm, false);
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
        let mut m = probe_machine(&kvm, false);
        m.abi = ABI_PAGE_RESEED;
        m.pause().unwrap();
        let saved = m.snapshot().unwrap();
        let before = m.published_entropy();
        m.reseed_time_page([42; ENTROPY_WORDS]).unwrap();
        assert!(m.is_paused());
        assert_eq!(m.time, 0);
        assert!(!m.is_parked());
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
