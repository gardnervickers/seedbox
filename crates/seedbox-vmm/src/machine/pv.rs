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

use super::pv_plan::clock_plan;
#[cfg(test)]
use super::PAGE_TIMER_DEADLINE;
use super::{Machine, Mapping, PAGE_BOUND, PAGE_SYSCALLS, PAGE_SYSCALL_TARGET};
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

/// The cost a throttle charges: the seeded `syscall_ns` times
/// `factor_permille / 1000`, rounded up so a throttle never speeds a machine up.
pub fn throttled_ns(syscall_ns: u64, factor_permille: u64) -> u64 {
    syscall_ns.saturating_mul(factor_permille).div_ceil(1000)
}
use seedbox_engine_core::pv_progress::{
    Accounting, Comparison, Decision, Exit as ProgressExit, LandingPlan, ProbeMachine,
    ProgressContext, PvProgress,
};

/// Per-thread PV execution context: the watchdog timer and per-run scratch.
/// Progress state belongs to each machine (`Machine::pv_progress`), so a
/// clock may serve any machine without carrying one machine's watchdog or
/// probe decisions into another's run.
pub struct PvClock {
    _watchdog: Watchdog,
    syscalls0: u64,
    remaining: u64,
    syscall_ns: u64,
    pub max_false_alarms: u64,
}
pub(crate) type Key = (u64, [u64; 17]);
/// A machine's spin-watchdog and probe state.
pub(crate) type Progress = PvProgress<Key, Saved>;
impl PvClock {
    pub fn new(watchdog: Duration) -> Result<Self, String> {
        Ok(Self {
            _watchdog: Watchdog::new(watchdog)?,
            syscalls0: 0,
            remaining: 0,
            syscall_ns: SYSCALL_NS,
            max_false_alarms: u64::MAX,
        })
    }

    /// Recreate host-only watchdog scratch after restoring a machine;
    /// `Machine::restore` resets the machine's own progress state.
    pub fn restore_boundary(&mut self) -> Result<(), String> {
        self.syscalls0 = 0;
        self.remaining = 0;
        self.syscall_ns = SYSCALL_NS;
        self._watchdog.hold(false)
    }

    fn plan(
        &mut self,
        m: &mut Machine,
        remaining: u64,
        soft: u64,
        legal: bool,
    ) -> Result<Prepared, String> {
        self.remaining = remaining;
        self.syscall_ns = match m.throttle_permille {
            None => m.syscall_ns,
            // An enabled page arm keeps the cost it was armed with until the
            // next legal plan, so a throttle or heal starts charging there.
            Some(_)
                if m.page_timer.enabled && !legal && m.page_get(super::PAGE_SYSCALL_NS) != 0 =>
            {
                m.page_get(super::PAGE_SYSCALL_NS)
            }
            Some(factor) => throttled_ns(m.syscall_ns, factor),
        };
        let mut progress = std::mem::take(&mut m.pv_progress);
        let decision = progress.prepare(m, self.max_false_alarms);
        m.pv_progress = progress;
        match decision? {
            Decision::Idle => return Ok(Prepared::Idle),
            Decision::Step => return Ok(Prepared::Run),
            Decision::Resume => (),
        }
        m.set_stepping(false)?;
        // An ABI-4 arm is a sequence of guest loads/stores, not an atomic
        // transaction. Never rewrite any of its inputs at a watchdog/probe
        // landing: even a mathematically equivalent replan can tear those
        // loads, relax a previous re-arm's bound, or move an overdue target.
        if let Some((bound, target)) = clock_plan(
            m.time,
            m.page_get(PAGE_SYSCALLS),
            self.syscall_ns,
            soft,
            m.page_timer.enabled,
            legal,
        ) {
            m.page_set(PAGE_BOUND, bound);
            m.page_set(PAGE_SYSCALL_TARGET, target);
            if m.page_timer.enabled {
                m.page_set(super::PAGE_SYSCALL_NS, self.syscall_ns);
                m.page_set(super::PAGE_SYSCALLS0, m.page_get(PAGE_SYSCALLS));
                m.page_set(super::PAGE_PLAN_TIME, m.time);
            }
        }
        Ok(Prepared::Run)
    }
}
fn accounting(stats: &mut super::Stats) -> Accounting<'_> {
    Accounting {
        probes: &mut stats.spin_probes,
        false_alarms: &mut stats.spin_false,
        clock_progress: &mut stats.spin_clock_progress,
        mismatches: &mut stats.spin_mismatch,
    }
}
impl ProgressContext for Machine {
    fn name(&self) -> &str {
        &self.name
    }
    fn accounting(&mut self) -> Accounting<'_> {
        accounting(&mut self.stats)
    }
}
impl ProbeMachine for Machine {
    type Key = Key;
    type Saved = Saved;
    const PC_NAME: &'static str = "rip";
    fn pc(key: &Key) -> u64 {
        key.0
    }
    fn key(&mut self) -> Result<Key, String> {
        // SAFETY: probes run only after `set_stepping(true)`, which fills the
        // synchronized registers, and each landing refreshes them.
        Ok(key(&unsafe { self.vcpu.get_kvm_run().s.regs.regs }))
    }
    fn clock_marker(&self) -> u64 {
        self.page_get(super::PAGE_LAST)
    }
    fn save(&mut self) -> Result<Saved, String> {
        Saved::take(self)
    }
    fn compare(&mut self, saved: &mut Saved) -> Result<Comparison, String> {
        saved.difference(self)
    }
    fn arm_next_landing(&mut self) -> Result<LandingPlan, String> {
        Ok(if self.arm_step()? {
            LandingPlan::PlannerCap
        } else {
            LandingPlan::Exact
        })
    }
    fn clear_debugger(&mut self) -> Result<(), String> {
        self.set_stepping(false)
    }
}
struct JudgeContext<'a> {
    name: &'a str,
    stats: &'a mut super::Stats,
}
impl ProgressContext for JudgeContext<'_> {
    fn name(&self) -> &str {
        self.name
    }
    fn accounting(&mut self) -> Accounting<'_> {
        accounting(self.stats)
    }
}

fn key(r: &kvm_bindings::kvm_regs) -> Key {
    let rflags = r.rflags & !(1 << 16); // RF is ours
    (
        r.rip,
        [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11,
            r.r12, r.r13, r.r14, r.r15, rflags,
        ],
    )
}

impl Timebase for PvClock {
    const BRANCHES: bool = false;

    fn prepare(
        &mut self,
        m: &mut Machine,
        remaining: u64,
        soft: u64,
        legal: bool,
    ) -> Result<Prepared, String> {
        let prepared = self.plan(m, remaining, soft, legal);
        // No tick may interrupt a probe step: an interrupt is taken before a
        // code breakpoint at the same instruction, so a tick there stops the
        // vCPU on a landing without reporting it, and stepping on past it
        // would miscount the cycle. Ticks wait until the check ends.
        self._watchdog.hold(m.pv_progress.hold_watchdog())?;
        prepared
    }
    fn start(&mut self, syscalls: u64) -> Result<u64, String> {
        self.syscalls0 = syscalls;
        Ok(0)
    }

    fn read(&mut self, syscalls: u64) -> Result<u64, String> {
        Ok((syscalls - self.syscalls0)
            .saturating_mul(self.syscall_ns)
            .min(self.remaining))
    }

    fn inject_guard(&self) -> u64 {
        0
    }

    fn judge(&mut self, j: Judge<'_>, run: &Run) -> Result<Verdict, String> {
        let exit = if run.interrupted {
            ProgressExit::Watchdog
        } else if run.debug {
            ProgressExit::Landing
        } else {
            ProgressExit::Natural
        };
        let legal = j.pv_progress.judge(
            exit,
            &mut JudgeContext {
                name: j.name,
                stats: j.stats,
            },
        )?;
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
                u.sync(&m._vm, m.mem.as_slice())?;
                None
            }
            None => {
                let mut ram = Mapping::new(m.mem.len())?;
                ram.as_mut_slice().copy_from_slice(m.mem.as_slice());
                Some(ram)
            }
        };
        Ok(Self {
            cpu,
            page: m.page.copy(),
            ram,
            written: Vec::new(),
        })
    }

    /// Compare the stopped architectural state. Shared proof policy records
    /// the first mismatch in this machine's accounting.
    fn difference(&mut self, m: &mut Machine) -> Result<Comparison, String> {
        let mut cpu = VcpuState::new(m.msrs.clone());
        cpu.save(&m.vcpu, "spin check")?;
        let page = m.page.copy();
        let part = match self.cpu.differs(&cpu) {
            Some(part) => Some(part),
            None => match self.page.iter().zip(page.iter()).position(|(a, b)| a != b) {
                Some(at) => Some(format!("time page word {}", at / 8)),
                None => {
                    let ram = match (&self.ram, m.undo.as_mut()) {
                        (None, Some(u)) => {
                            u.unchanged_since_sync(&m._vm, m.mem.as_slice(), &mut self.written)?
                        }
                        (Some(ram), _) => ram.as_slice() == m.mem.as_slice(),
                        (None, None) => return Err("spin check: RAM not saved".into()),
                    };
                    (!ram).then(|| "ram".to_string())
                }
            },
        };
        Ok(match part {
            None => Comparison::Equal,
            Some(part) => Comparison::Different(part),
        })
    }
}

/// A periodic signal to this thread, every `period` of its CPU time, so a
/// KVM_RUN that makes no exit returns with EINTR.
struct Watchdog(libc::timer_t);

extern "C" fn on_tick(_: libc::c_int) {}

impl Watchdog {
    fn new(period: Duration) -> Result<Self, String> {
        let err = |what: &str| format!("pv watchdog {what}: {}", std::io::Error::last_os_error());
        // SAFETY: the handler does nothing; every structure passed is zeroed
        // or initialized here and outlives its call.
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
            let spec = libc::itimerspec {
                it_interval: ts,
                it_value: ts,
            };
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
        // SAFETY: changes only this thread's mask, from a set built here.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGUSR2);
            let how = if on {
                libc::SIG_BLOCK
            } else {
                libc::SIG_UNBLOCK
            };
            match libc::pthread_sigmask(how, &set, std::ptr::null_mut()) {
                0 => Ok(()),
                e => Err(format!(
                    "pv watchdog mask: {}",
                    std::io::Error::from_raw_os_error(e)
                )),
            }
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // SAFETY: the timer `new` created; nothing uses it after drop.
        unsafe { libc::timer_delete(self.0) };
        let _ = self.hold(false);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BootSpec, Stop, INITRD_ADDR};
    use super::*;
    use kvm_ioctls::Kvm;

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn guest_without_page_timer_support_fails_at_boot() {
        let kvm = Kvm::new().unwrap();
        for requested in [false, true] {
            for abi in 0..=super::super::ABI_PAGE_TIMER {
                let mut code = vec![0xb8];
                code.extend_from_slice(&(abi as u32).to_le_bytes());
                code.extend_from_slice(&[0xba, 0x28, 0x05, 0, 0, 0xef, 0xf4]);
                let mut m = machine(&kvm, &code, 0);
                m.page_timer_requested = requested;
                m.page_set(super::super::PAGE_PV_CLOCK, 2);
                m.pic.set_masks(0xffff);
                let mut clock = PvClock::new(Duration::from_secs(10)).unwrap();
                let got = m.run_until(&mut clock, 1000000, 0, &mut Vec::new());
                if requested && abi < super::super::ABI_PAGE_TIMER {
                    assert_eq!(got.unwrap_err(), "the guest kernel lacks page timer support; rebuild it from kernel/ or drop --page-timer");
                    assert!(!m.page_timer.enabled);
                    assert_eq!(m.page_get(super::super::PAGE_CAPABILITIES), 0);
                } else {
                    assert_eq!(got.unwrap(), Stop::Halted);
                    assert_eq!(m.page_timer.enabled, requested);
                }
            }
        }
    }

    /// A 32-bit payload at the kernel entry, interrupts enabled and unmasked.
    fn machine(kvm: &Kvm, code: &[u8], undo: u64) -> Machine {
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
            pv_clock: true,
            page_timer: false,
        };
        let mut m = Machine::new(kvm, "pv-probe", &spec).unwrap();
        let mut r = m.vcpu.get_regs().unwrap();
        r.rflags = 0x202;
        m.vcpu.set_regs(&r).unwrap();
        m.pic.set_masks(0);
        m
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn a_page_arm_lowers_the_running_guest_clock_bound() {
        let kvm = Kvm::new().unwrap();
        let mut code = Vec::new();
        // Store a 20 us deadline and lower bound while KVM is running.
        for (address, value) in [
            (0xd0000070u32, 20000u32),
            (0xd0000074, 0),
            (0xd0000038, 20000),
        ] {
            code.extend_from_slice(&[0xc7, 0x05]);
            code.extend_from_slice(&address.to_le_bytes());
            code.extend_from_slice(&value.to_le_bytes());
        }
        // Review repro: 2^24 branches between the stores and the yield.
        code.extend_from_slice(&[0xb9, 0, 0, 0, 0x01, 0x49, 0x75, 0xfd]);
        code.extend_from_slice(&[0xba, 0x2c, 0x05, 0, 0, 0xb0, 1, 0xee, 0xf4]);
        for watchdog in [Duration::from_secs(10), Duration::from_micros(100)] {
            let mut m = machine(&kvm, &code, 0);
            m.page_timer_requested = true;
            m.abi = super::super::ABI_PAGE_TIMER;
            m.enable_page_timer();
            m.pic.set_masks(0xffff); // Retain IRQ0 rather than entering an absent IDT.
            let mut clock = PvClock::new(watchdog).unwrap();
            let stop = m
                .run_until(&mut clock, 1000000, 0, &mut Vec::new())
                .unwrap();
            assert_eq!(
                (stop, m.time),
                (Stop::Halted, 20000),
                "watchdog={watchdog:?}"
            );
            assert_eq!(m.pit.deadline, None);
            m.pic.set_masks(0);
            assert_eq!(m.pic.pending(), Some(0));
        }
    }

    #[test]
    fn throttled_cost_scales_the_seeded_speed_and_rounds_up() {
        assert_eq!(throttled_ns(937, 1000), 937);
        assert_eq!(throttled_ns(937, 2500), 2343);
        assert_eq!(throttled_ns(1100, 1_000_000), 1_100_000);
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn a_throttle_and_its_heal_charge_from_the_next_legal_plan() {
        let kvm = Kvm::new().unwrap();
        let mut m = machine(&kvm, &[0xf4], 0);
        m.page_timer_requested = true;
        m.abi = super::super::ABI_PAGE_TIMER;
        m.enable_page_timer();
        m.syscall_ns = 937;
        let mut clock = PvClock::new(Duration::from_secs(10)).unwrap();
        m.page_set(PAGE_SYSCALLS, 17);
        let mut charge = |m: &mut Machine, legal: bool| {
            clock.prepare(m, 1_000_000, 1_000_000, legal).unwrap();
            clock.start(17).unwrap();
            (
                clock.read(27).unwrap(),
                m.page_get(super::super::PAGE_SYSCALL_NS),
            )
        };
        assert_eq!(charge(&mut m, true), (9370, 937));
        m.throttle_permille = Some(4000);
        // A watchdog landing keeps the cost the guest's arm was planned with.
        assert_eq!(charge(&mut m, false), (9370, 937));
        assert_eq!(charge(&mut m, true), (37480, 3748));
        m.throttle_permille = Some(1000);
        assert_eq!(charge(&mut m, false), (37480, 3748));
        assert_eq!(charge(&mut m, true), (9370, 937));
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn watchdog_plans_do_not_adopt_expire_or_draw_page_timer_latency() {
        let kvm = Kvm::new().unwrap();
        let mut m = machine(&kvm, &[0xf4], 0);
        m.page_timer_requested = true;
        m.abi = super::super::ABI_PAGE_TIMER;
        m.enable_page_timer();
        m.pit.program_deadline(Some(10000));
        m.pit.fresh = false;
        m.timer_rng = Some(crate::rng::Rng::new(1009, "timer/pv-probe"));
        m.timer_jitter = 1000;
        let mut clock = PvClock::new(Duration::from_secs(10)).unwrap();
        m.page_set(PAGE_SYSCALLS, 17);
        clock.prepare(&mut m, 100000, 100000, true).unwrap();
        let fields = [
            PAGE_BOUND,
            PAGE_SYSCALL_TARGET,
            super::super::PAGE_SYSCALL_NS,
            super::super::PAGE_SYSCALLS0,
            super::super::PAGE_PLAN_TIME,
        ];
        let plan = fields.map(|i| m.page_get(i));
        // Simulate watchdog landings before/after an old arm is overdue.
        // Even a split between guest plan loads must keep every field fixed.
        for time in [0, 9999, 10000, 10293] {
            m.time = time;
            m.page_set(PAGE_SYSCALLS, 24);
            for word in [5000, 20000, super::super::TIMER_DISARMED] {
                m.page_set(PAGE_TIMER_DEADLINE, word);
                assert!(matches!(
                    clock.prepare(&mut m, 100000, 100000, false).unwrap(),
                    Prepared::Run
                ));
                assert_eq!(
                    fields.map(|i| m.page_get(i)),
                    plan,
                    "watchdog cannot tear a guest arm's plan loads"
                );
                assert_eq!(m.page_timer.seen, super::super::TIMER_DISARMED);
                assert_eq!(m.pit.deadline, Some(10000));
                assert_eq!(m.pic.pending(), None);
                assert_eq!(m.timer_rng.as_ref().unwrap().counter(), 0);
                assert_eq!(
                    m.next_deadline(),
                    10000,
                    "idle wake must ignore the raw word"
                );
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn store_deadline_then_halt_has_the_same_wake_irq_and_witness_across_watchdogs() {
        let kvm = Kvm::new().unwrap();
        for (spin, old, word) in [
            (false, None, 900000u64),
            (false, Some(20000), 900000),
            (false, None, 20000),
            (false, Some(20000), super::super::TIMER_DISARMED),
            (true, None, 900000),
        ] {
            let mut code = Vec::new();
            // Clock reads have advanced last to 30 us. Store the deadline,
            // compute for long enough to encounter watchdog splits, then HLT.
            for (address, value) in [
                (0xd0000008u32, 30000u32),
                (0xd000000c, 0),
                (0xd0000070, word as u32),
                (0xd0000074, (word >> 32) as u32),
            ] {
                code.extend_from_slice(&[0xc7, 0x05]);
                code.extend_from_slice(&address.to_le_bytes());
                code.extend_from_slice(&value.to_le_bytes());
            }
            if spin {
                code.extend_from_slice(&[0xeb, 0xfe]); // Canonical spin idle is also a legal adoption point.
            } else {
                code.extend_from_slice(&[0xb9, 0, 0, 0, 0x01, 0x49, 0x75, 0xfd, 0xf4]);
            }
            let mut want = None;
            for (watchdog, undo) in [
                (
                    if spin {
                        Duration::from_millis(2)
                    } else {
                        Duration::from_secs(10)
                    },
                    0,
                ),
                (Duration::from_micros(100), 0),
                (Duration::from_micros(200), 256),
            ] {
                let mut m = machine(&kvm, &code, undo);
                m.page_timer_requested = true;
                m.abi = super::super::ABI_PAGE_TIMER;
                m.enable_page_timer();
                m.pit.program_deadline(old);
                m.pit.fresh = false;
                m.tolerance = 100000;
                m.timer_jitter = 1000;
                m.exact_timer_ppm = 500000;
                m.timer_rng = Some(crate::rng::Rng::new(1009, "timer/pv-probe"));
                // Keep IRQ0 pending until HLT: this payload has no IDT.
                m.pic.set_masks(0xffff);
                let mut clock = PvClock::new(watchdog).unwrap();
                assert_eq!(
                    m.run_until(&mut clock, 1000000, 0, &mut Vec::new())
                        .unwrap(),
                    Stop::Halted
                );
                assert_eq!(m.time, 30000, "idle must include page-clock progress");
                assert_eq!(m.stats.spin_idles, u64::from(spin));
                if watchdog < Duration::from_secs(1) {
                    assert!(
                        m.stats.eintr > 0,
                        "the short watchdog must actually interrupt the guest"
                    );
                }
                m.pic.set_masks(0);
                let pending = old.is_some() || word <= 30000;
                assert_eq!(
                    m.pic.pending(),
                    pending.then_some(0),
                    "overdue arms must survive replacement/disarm"
                );
                let adopted = m.next_deadline();
                let wake = if pending { m.time } else { adopted };
                if !pending {
                    assert!((word..=word + 1000).contains(&wake));
                }
                // Model the world's halted key and wake, then expire and inject
                // once without entering the guest's absent interrupt handler.
                m.wake(wake);
                m.expire_timers().unwrap();
                m.inject().unwrap();
                assert_eq!(m.stats.irqs, 1);
                assert_eq!(m.pic.pending(), None);
                let mut rng = m.timer_rng.as_ref().unwrap().clone();
                let got = (wake, adopted, m.stats.irqs, m.exit_witness(), rng.next());
                if let Some(want) = &want {
                    assert_eq!(&got, want, "watchdog={watchdog:?} undo={undo}");
                } else {
                    want = Some(got);
                }
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn changing_idle_periods_keep_plan_and_wake_across_watchdogs() {
        let kvm = Kvm::new().unwrap();
        let start = 1_010_599_858u64;
        let periods = [(100_001, 500_003), (700_007, 300_011), (200_013, 800_017)];
        // No IDT: retain IRQ0 in the PIC, observe it, then acknowledge between
        // rounds. HLT still has IF set (machine() uses rflags=0x202).
        let store = |code: &mut Vec<u8>, address: u32, value: u64| {
            for (address, value) in [(address, value as u32), (address + 4, (value >> 32) as u32)] {
                code.extend_from_slice(&[0xc7, 0x05]);
                code.extend_from_slice(&address.to_le_bytes());
                code.extend_from_slice(&value.to_le_bytes());
            }
        };
        let delay = [0xb9, 0, 0, 0, 0x01, 0x49, 0x75, 0xfd];
        let mut code = Vec::new();
        for (round, (first, second)) in periods.into_iter().enumerate() {
            let origin = start + round as u64 * 2_000_000;
            code.extend_from_slice(&delay); // splits before the first arm
            store(&mut code, 0xd0000070, origin + first);
            store(&mut code, 0xd0000038, origin + first);
            code.extend_from_slice(&delay); // splits while the first bound is lowered
            store(&mut code, 0xd0000070, origin + second);
            // A later re-arm retains the earlier bound, as seedbox.c does.
            store(&mut code, 0xd0000038, origin + first.min(second));
            code.extend_from_slice(&delay); // splits after re-arm, before idle
            code.extend_from_slice(&[0xba, 0x2c, 0x05, 0, 0, 0xb0, 1, 0xee, 0xf4]);
        }
        let mut want = None;
        for (watchdog, undo) in [
            (Duration::from_secs(10), 0),
            (Duration::from_micros(100), 0),
            (Duration::from_micros(700), 256),
        ] {
            let mut m = machine(&kvm, &code, undo);
            m.page_timer_requested = true;
            m.abi = super::super::ABI_PAGE_TIMER;
            m.enable_page_timer();
            m.pic.set_masks(0xffff);
            m.timer_jitter = 1000;
            m.exact_timer_ppm = 500000;
            m.timer_rng = Some(crate::rng::Rng::new(1009, "timer/pv-probe"));
            let mut clock = PvClock::new(watchdog).unwrap();
            let mut evidence = Vec::new();
            for (round, (first, second)) in periods.into_iter().enumerate() {
                let origin = start + round as u64 * 2_000_000;
                m.wake(origin);
                assert_eq!(
                    m.run_until(&mut clock, origin + 1_000_000, 0, &mut Vec::new())
                        .unwrap(),
                    Stop::Halted
                );
                assert_eq!(m.time, origin + first.min(second));
                let adopted = m.next_deadline();
                assert!((origin + second..=origin + second + 1000).contains(&adopted));
                // A halted world does not enter KVM. Even hypothetical repeated
                // watchdog preparation here must leave its wake and plan intact.
                let plan = [
                    PAGE_BOUND,
                    PAGE_SYSCALL_TARGET,
                    super::super::PAGE_SYSCALLS0,
                    super::super::PAGE_PLAN_TIME,
                ]
                .map(|i| m.page_get(i));
                for _ in 0..round + 1 {
                    clock
                        .prepare(&mut m, 2_000_000, origin + 2_000_000, false)
                        .unwrap();
                    assert_eq!(m.next_deadline(), adopted);
                    assert_eq!(
                        [
                            PAGE_BOUND,
                            PAGE_SYSCALL_TARGET,
                            super::super::PAGE_SYSCALLS0,
                            super::super::PAGE_PLAN_TIME
                        ]
                        .map(|i| m.page_get(i)),
                        plan
                    );
                }
                evidence.push((
                    m.time,
                    m.pic.pending(),
                    m.pit.deadline,
                    m.exit_witness(),
                    m.timer_rng.as_ref().unwrap().counter(),
                ));
                m.wake(adopted);
                m.expire_timers().unwrap();
                m.pic.set_masks(0);
                assert_eq!(m.pic.pending(), Some(0));
                evidence.push((
                    m.time,
                    m.pic.pending(),
                    m.pit.deadline,
                    m.exit_witness(),
                    m.timer_rng.as_ref().unwrap().counter(),
                ));
                m.pic.take(0);
                m.pic.set_masks(0xffff);
            }
            if watchdog < Duration::from_secs(1) {
                assert!(m.stats.eintr > 0);
            }
            if let Some(want) = &want {
                assert_eq!(&evidence, want, "watchdog={watchdog:?} undo={undo}");
            } else {
                want = Some(evidence);
            }
        }
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
            let stop = m
                .run_until(&mut clock, 1_000_000_000, 0, &mut Vec::new())
                .unwrap();
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
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn a_jump_to_itself_is_idle_at_the_jump() {
        let (stop, rip, _, m) = run(&[0xeb, 0xfe], true); // jmp $
        assert_eq!(stop, Stop::Halted);
        assert_eq!(rip, 0x100000);
        assert_eq!(m.time, 0, "compute costs no virtual time");
        assert_eq!(m.stats.spin_idles, 1);
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
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
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn frequent_ticks_do_not_disturb_a_spin_check() {
        let kvm = Kvm::new().unwrap();
        let code = [0xb9, 5, 0, 0, 0, 0x49, 0x75, 0xfd, 0xeb, 0xf6];
        for _ in 0..20 {
            let mut clock = PvClock::new(Duration::from_micros(200)).unwrap();
            clock.max_false_alarms = 10;
            let mut m = machine(&kvm, &code, 0);
            assert_eq!(
                m.run_until(&mut clock, 1_000_000_000, 0, &mut Vec::new())
                    .unwrap(),
                Stop::Halted
            );
            let r = m.vcpu.get_regs().unwrap();
            assert_eq!((r.rip, r.rcx as u32, m.time), (0x100005, 1, 0));
            assert_eq!(m.stats.spin_idles, 1);
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn a_long_finite_loop_is_left_alone() {
        // mov ecx,0x20000000; l: dec ecx; jnz l; hlt
        let (stop, rip, rcx, m) = run(&[0xb9, 0, 0, 0, 0x20, 0x49, 0x75, 0xfd, 0xf4], false);
        assert_eq!(stop, Stop::Halted);
        assert_eq!((rip, rcx as u32), (0x100009, 0));
        assert_eq!(m.stats.spin_idles, 0);
        assert!(
            m.stats.spin_probes > 0,
            "the loop should outlast the watchdog"
        );
        assert_eq!(m.stats.spin_probes, m.stats.spin_false);
    }
}
