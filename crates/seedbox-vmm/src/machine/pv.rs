//! The PV clock timebase: no host perf counter. Compute between exits costs
//! no virtual time (the infinite-CPU model); time moves only at
//!
//! - exits the guest makes anyway (I/O, MMIO, HLT), with their usual costs,
//! - guest clock reads, `SEEDBOX_PV_READ_NS` (100 ns) each, against 1 ns in
//!   the counter timebase, where branches move time between reads
//!   (`seedbox_rdtsc`),
//! - syscalls, counted by the guest in the time page, each costing the
//!   machine's `Machine::syscall_ns`: `SYSCALL_NS` scaled by a seeded
//!   per-machine speed (see `syscall_ns`).
//!
//! So a run needs no counted stop: the guest kernel exits by itself when its
//! clock would pass the soft stop (the `bound` we publish) or at the syscall
//! that reaches it (`syscall_target`), both behind a static key that is off in
//! the counter timebase. Every exit is a deterministic point.
//!
//! A loop with no exit at all would never end and never let time move. A
//! watchdog (host CPU time, a performance setting) catches a run with no exit
//! for a while and steps it branch by branch. If the whole machine state
//! repeats, the loop provably never ends by itself: the machine steps to a
//! canonical point of the cycle and is idle there, like HLT, until an
//! interrupt. A change to the time page's `last` since the probe started
//! proves clock progress, so the probe gives up immediately. Any other long
//! run resumes untouched (probing changes no guest-visible state; giving up
//! only resumes the run), so the watchdog changes how fast a spin is noticed,
//! never where it lands. Not every endless loop is caught: one whose state
//! changes on every pass (`while (!flag) n++;`), a cycle longer than
//! about a third of `PROBE_STEPS` landings, or one with a run of straight-line
//! code longer than the planner's cap
//! never goes idle, so virtual time stops and the world stalls until its
//! wall-time limit. That follows from the model: such a loop waits on
//! something only virtual time could bring.

use super::{map, Machine, Mapping, PAGE_BOUND, PAGE_SYSCALLS, PAGE_SYSCALL_TARGET};
use crate::timebase::{Judge, Prepared, Run, Timebase, Verdict};
use crate::undo::VcpuState;
use std::time::Duration;

/// Nominal virtual time one syscall costs. Part of the PV clock's semantics.
pub const SYSCALL_NS: u64 = 1000;
/// Spread of per-machine syscall cost around `SYSCALL_NS`, in percent.
pub const SPEED_SPREAD_PCT: u64 = 10;

/// One machine's syscall cost, drawn from `rng`: uniform within
/// `SPEED_SPREAD_PCT` of `SYSCALL_NS`.
///
/// Compute is free in this model, so identical machines booting the same
/// image would otherwise make the same syscalls at the same nanoseconds
/// forever. Real machines never run in lockstep, and software relies on it:
/// etcd seeds raft's election timeouts from the clock, and three members in
/// lockstep split every vote. A seeded speed per machine, like the varying
/// cost of real CPUs, separates them.
pub fn syscall_ns(rng: &mut crate::rng::Rng) -> u64 {
    let spread = SYSCALL_NS * SPEED_SPREAD_PCT / 100;
    SYSCALL_NS - spread + rng.below(2 * spread + 1)
}
use seedbox_engine_core::pv_progress::{PvProgress, ProbeMachine, ProgressContext, Accounting, Comparison, LandingPlan, Decision, Exit as ProgressExit};

pub struct PvClock {
    _watchdog: Watchdog,
    progress: PvProgress<Key, Saved>,
    syscalls0: u64,
    remaining: u64,
    syscall_ns: u64,
    pub max_false_alarms: u64,
}
type Key = (u64, [u64; 17]);
impl PvClock {
    pub fn new(watchdog: Duration) -> Result<Self, String> {
        Ok(Self { _watchdog: Watchdog::new(watchdog)?, progress: PvProgress::new(), syscalls0: 0, remaining: 0, syscall_ns: SYSCALL_NS, max_false_alarms: u64::MAX })
    }
    fn plan(&mut self, m: &mut Machine, remaining: u64, soft: u64) -> Result<Prepared, String> {
        self.remaining = remaining;
        self.syscall_ns = m.syscall_ns;
        match self.progress.prepare(m, self.max_false_alarms)? {
            Decision::Idle => return Ok(Prepared::Idle),
            Decision::Step => return Ok(Prepared::Run),
            Decision::Resume => (),
        }
        m.page_set(PAGE_BOUND, soft);
        let syscalls = m.page_get(PAGE_SYSCALLS);
        let left = soft.saturating_sub(m.time).div_ceil(self.syscall_ns).max(1);
        m.page_set(PAGE_SYSCALL_TARGET, syscalls + left);
        Ok(Prepared::Run)
    }
}
fn accounting(stats: &mut super::Stats) -> Accounting<'_> {
    Accounting { probes: &mut stats.spin_probes, false_alarms: &mut stats.spin_false, clock_progress: &mut stats.spin_clock_progress, mismatches: &mut stats.spin_mismatch }
}
impl ProgressContext for Machine {
    fn name(&self) -> &str { &self.name }
    fn accounting(&mut self) -> Accounting<'_> { accounting(&mut self.stats) }
}
impl ProbeMachine for Machine {
    type Key = Key;
    type Saved = Saved;
    const PC_NAME: &'static str = "rip";
    fn pc(key: &Key) -> u64 { key.0 }
    fn key(&mut self) -> Result<Key,String> { Ok(key(&unsafe { self.vcpu.get_kvm_run().s.regs.regs })) }
    fn clock_marker(&self) -> u64 { self.page_get(1) }
    fn save(&mut self) -> Result<Saved,String> { Saved::take(self) }
    fn compare(&mut self, saved: &mut Saved) -> Result<Comparison,String> { saved.difference(self) }
    fn arm_next_landing(&mut self) -> Result<LandingPlan,String> { Ok(if self.arm_step()? { LandingPlan::PlannerCap } else { LandingPlan::Exact }) }
    fn clear_debugger(&mut self) -> Result<(),String> { self.set_stepping(false) }
}
struct JudgeContext<'a> { name: &'a str, stats: &'a mut super::Stats }
impl ProgressContext for JudgeContext<'_> {
    fn name(&self) -> &str { self.name }
    fn accounting(&mut self) -> Accounting<'_> { accounting(self.stats) }
}

fn key(r: &kvm_bindings::kvm_regs) -> Key {
    let rflags = r.rflags & !(1 << 16); // RF is ours
    (
        r.rip,
        [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11, r.r12,
            r.r13, r.r14, r.r15, rflags,
        ],
    )
}

impl Timebase for PvClock {
    const BRANCHES: bool = false;

    fn prepare(&mut self, m: &mut Machine, remaining: u64, soft: u64) -> Result<Prepared, String> {
        let prepared = self.plan(m, remaining, soft);
        // No tick may interrupt a probe step: an interrupt is taken before a
        // code breakpoint at the same instruction, so a tick there stops the
        // vCPU on a landing without reporting it, and stepping on past it
        // would miscount the cycle. Ticks wait until the check ends.
        self._watchdog.hold(self.progress.hold_watchdog())?;
        prepared
    }
    fn start(&mut self, syscalls: u64) -> u64 {
        self.syscalls0 = syscalls;
        0
    }

    fn read(&mut self, syscalls: u64) -> u64 {
        (syscalls - self.syscalls0).saturating_mul(self.syscall_ns).min(self.remaining)
    }

    fn inject_guard(&self) -> u64 {
        0
    }

    fn judge(&mut self, j: Judge<'_>, run: &Run) -> Result<Verdict, String> {
        let exit = if run.interrupted { ProgressExit::Watchdog } else if run.debug { ProgressExit::Landing } else { ProgressExit::Natural };
        let legal = self.progress.judge(exit, &mut JudgeContext { name: j.name, stats: j.stats })?;
        Ok(Verdict::Advance { legal })
    }
}

/// The machine state a cycle check compares.
pub struct Saved {
    cpu: VcpuState,
    page: Box<[u8; 4096]>,
    /// A copy of RAM, when no dirty log tracks it.
    ram: Option<Mapping>,
    /// With a dirty log: the pages written since this state was taken.
    written: Vec<u64>,
}

impl Saved {
    fn take(m: &mut Machine) -> Result<Self, String> {
        let mut cpu = VcpuState::new(m.msrs.clone());
        cpu.save(&m.vcpu, "spin check")?;
        let ram = match m.undo.as_mut() {
            Some(u) => {
                u.sync(&m._vm, m.mem)?;
                None
            }
            None => {
                let ram = Mapping { ptr: map(m.mem_size)?, size: m.mem_size };
                unsafe { std::ptr::copy_nonoverlapping(m.mem, ram.ptr, m.mem_size) };
                Some(ram)
            }
        };
        Ok(Self { cpu, page: page_copy(m), ram, written: Vec::new() })
    }

    /// Compare the stopped architectural state. Shared proof policy records
    /// the first mismatch in this machine's accounting.
    fn difference(&mut self, m: &mut Machine) -> Result<Comparison, String> {
        let mut cpu = VcpuState::new(m.msrs.clone());
        cpu.save(&m.vcpu, "spin check")?;
        let page = page_copy(m);
        let part = match self.cpu.differs(&cpu) {
            Some(part) => Some(part),
            None => match self.page.iter().zip(page.iter()).position(|(a, b)| a != b) {
                Some(at) => Some(format!("time page word {}", at / 8)),
                None => {
                    let ram = match (&self.ram, m.undo.as_mut()) {
                        (None, Some(u)) => u.unchanged_since_sync(&m._vm, m.mem, &mut self.written)?,
                        (Some(ram), _) => unsafe {
                            std::slice::from_raw_parts(ram.ptr, ram.size)
                                == std::slice::from_raw_parts(m.mem, m.mem_size)
                        },
                        (None, None) => return Err("spin check: RAM not saved".into()),
                    };
                    (!ram).then(|| "ram".to_string())
                }
            },
        };
        Ok(match part { None => Comparison::Equal, Some(part) => Comparison::Different(part) })
    }
}

fn page_copy(m: &Machine) -> Box<[u8; 4096]> {
    let mut page = Box::new([0u8; 4096]);
    unsafe { std::ptr::copy_nonoverlapping(m.page as *const u8, page.as_mut_ptr(), 4096) };
    page
}

/// A periodic signal to this thread, every `period` of its CPU time, so a
/// KVM_RUN that makes no exit returns with EINTR.
struct Watchdog(libc::timer_t);

extern "C" fn on_tick(_: libc::c_int) {}

impl Watchdog {
    fn new(period: Duration) -> Result<Self, String> {
        let err = |what: &str| format!("pv watchdog {what}: {}", std::io::Error::last_os_error());
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_tick as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut()) != 0 {
                return Err(err("sigaction"));
            }
            let mut sev: libc::sigevent = std::mem::zeroed();
            sev.sigev_notify = libc::SIGEV_THREAD_ID;
            sev.sigev_signo = libc::SIGUSR2;
            sev.sigev_notify_thread_id = libc::syscall(libc::SYS_gettid) as i32;
            let mut timer: libc::timer_t = std::ptr::null_mut();
            if libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut sev, &mut timer) != 0 {
                return Err(err("timer"));
            }
            let w = Self(timer);
            let ts = libc::timespec {
                tv_sec: period.as_secs() as libc::time_t,
                tv_nsec: period.subsec_nanos() as libc::c_long,
            };
            let spec = libc::itimerspec { it_interval: ts, it_value: ts };
            if libc::timer_settime(timer, 0, &spec, std::ptr::null_mut()) != 0 {
                return Err(err("arm"));
            }
            Ok(w)
        }
    }
}

impl Watchdog {
    /// Hold back ticks on this thread (they stay pending), or let them in.
    fn hold(&self, on: bool) -> Result<(), String> {
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGUSR2);
            let how = if on { libc::SIG_BLOCK } else { libc::SIG_UNBLOCK };
            match libc::pthread_sigmask(how, &set, std::ptr::null_mut()) {
                0 => Ok(()),
                e => Err(format!("pv watchdog mask: {}", std::io::Error::from_raw_os_error(e))),
            }
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        unsafe { libc::timer_delete(self.0) };
        let _ = self.hold(false);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BootSpec, Stop, INITRD_ADDR};
    use super::*;
    use kvm_ioctls::Kvm;

    /// A 32-bit payload at the kernel entry, interrupts enabled and unmasked.
    fn machine(kvm: &Kvm, code: &[u8], undo: u64) -> Machine {
        let mut kernel = vec![0u8; 1024];
        kernel[0x1f1] = 1;
        kernel.extend(code);
        let spec = BootSpec {
            kernel: &kernel, initrd: &[], cmdline: String::new(), mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32], mac: None, control_channel: false, disk: None, undo_margin: undo, barrier: false,
            pv_clock: true,
        };
        let mut m = Machine::new(kvm, "pv-probe", &spec).unwrap();
        let mut r = m.vcpu.get_regs().unwrap();
        r.rflags = 0x202;
        m.vcpu.set_regs(&r).unwrap();
        m.pic.set_masks(0);
        m
    }

    /// Run to the first stop under several watchdog periods and undo
    /// settings; all must agree. Returns the stop, rip, rcx and the machine.
    /// `idle`: the guest should go idle, so fail after a few checks that
    /// find no cycle rather than spin until the test times out.
    fn run(code: &[u8], idle: bool) -> (Stop, u64, u64, Machine) {
        let kvm = Kvm::new().unwrap();
        let mut first: Option<(u64, u64, u64, String)> = None;
        let mut last = None;
        for (ms, undo) in [(2, 256), (7, 0), (3, 0)] {
            let mut clock = PvClock::new(Duration::from_millis(ms)).unwrap();
            if idle {
                clock.max_false_alarms = 10;
            }
            let mut m = machine(&kvm, code, undo);
            let stop = m.run_until(&mut clock, 1_000_000_000, 0, &mut Vec::new()).unwrap();
            let r = m.vcpu.get_regs().unwrap();
            let evidence = (m.time, r.rip, r.rcx, m.exit_witness());
            if let Some(want) = &first {
                assert_eq!(&evidence, want, "watchdog {ms} ms undo {undo}");
            } else {
                first = Some(evidence);
            }
            last = Some((stop, r.rip, r.rcx, m));
        }
        last.unwrap()
    }

    #[test]
    #[ignore = "requires KVM"]
    fn a_jump_to_itself_is_idle_at_the_jump() {
        let (stop, rip, _, m) = run(&[0xeb, 0xfe], true); // jmp $
        assert_eq!(stop, Stop::Halted);
        assert_eq!(rip, 0x100000);
        assert_eq!(m.time, 0, "compute costs no virtual time");
        assert_eq!(m.stats.spin_idles, 1);
    }

    #[test]
    #[ignore = "requires KVM"]
    fn a_cycle_with_an_inner_loop_is_idle_at_its_canonical_point() {
        // outer: mov ecx,5; inner: dec ecx; jnz inner; jmp outer
        let (stop, rip, rcx, m) = run(&[0xb9, 5, 0, 0, 0, 0x49, 0x75, 0xfd, 0xeb, 0xf6], true);
        assert_eq!(stop, Stop::Halted);
        // The least landing: inner head, with ecx at its least there.
        assert_eq!((rip, rcx as u32), (0x100005, 1));
        assert_eq!(m.stats.spin_idles, 1);
    }

    #[test]
    fn ticks_wait_while_held() {
        let pending = || unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigpending(&mut set);
            libc::sigismember(&set, libc::SIGUSR2) == 1
        };
        let w = Watchdog::new(Duration::from_millis(1)).unwrap();
        w.hold(true).unwrap();
        let t = std::time::Instant::now();
        while !pending() {
            assert!(t.elapsed() < Duration::from_secs(5), "no tick arrived");
        }
        w.hold(false).unwrap();
        assert!(!pending(), "the held tick is delivered once let in");
    }

    /// Ticks far more often than a check lasts: every check still counts
    /// each landing, so all runs idle at the same point after one check.
    #[test]
    #[ignore = "requires KVM"]
    fn frequent_ticks_do_not_disturb_a_spin_check() {
        let kvm = Kvm::new().unwrap();
        let code = [0xb9, 5, 0, 0, 0, 0x49, 0x75, 0xfd, 0xeb, 0xf6];
        for _ in 0..20 {
            let mut clock = PvClock::new(Duration::from_micros(200)).unwrap();
            clock.max_false_alarms = 10;
            let mut m = machine(&kvm, &code, 0);
            assert_eq!(m.run_until(&mut clock, 1_000_000_000, 0, &mut Vec::new()).unwrap(), Stop::Halted);
            let r = m.vcpu.get_regs().unwrap();
            assert_eq!((r.rip, r.rcx as u32, m.time), (0x100005, 1, 0));
            assert_eq!(m.stats.spin_idles, 1);
        }
    }

    #[test]
    #[ignore = "requires KVM"]
    fn a_long_finite_loop_is_left_alone() {
        // mov ecx,0x20000000; l: dec ecx; jnz l; hlt
        let (stop, rip, rcx, m) = run(&[0xb9, 0, 0, 0, 0x20, 0x49, 0x75, 0xfd, 0xf4], false);
        assert_eq!(stop, Stop::Halted);
        assert_eq!((rip, rcx as u32), (0x100009, 0));
        assert_eq!(m.stats.spin_idles, 0);
        assert!(m.stats.spin_probes > 0, "the loop should outlast the watchdog");
        assert_eq!(m.stats.spin_probes, m.stats.spin_false);
    }
}
