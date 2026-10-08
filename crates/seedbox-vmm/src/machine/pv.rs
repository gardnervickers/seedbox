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

use super::{map, Machine, Mapping, PAGE_BOUND, PAGE_SYSCALLS, PAGE_SYSCALL_TARGET, PAGE_TIMER_DEADLINE};
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
/// Steps one spin check may take before it gives up.
const PROBE_STEPS: usize = 4096;

pub struct PvClock {
    /// Held for its timer.
    _watchdog: Watchdog,
    /// A natural exit happened since the last watchdog tick.
    progressed: bool,
    /// Ticks to let pass before the next check, after checks that found no
    /// spin in a run that still has made no exit (doubling each time).
    skip: u32,
    backoff: u32,
    syscalls0: u64,
    remaining: u64,
    /// The running machine's syscall cost.
    syscall_ns: u64,
    probe: Option<Probe>,
    /// The vCPU stands at a step landing the probe has not seen yet.
    landed: bool,
    /// Fail rather than run on after this many checks found no spin (tests).
    pub max_false_alarms: u64,
}

/// A step landing: RIP, then the general registers and flags.
type Key = (u64, [u64; 17]);

struct Probe {
    /// Captured before the first step, kept across candidate cycles.
    last: Option<u64>,
    phase: Phase,
}

impl Probe {
    fn new() -> Self {
        Self { last: None, phase: Phase::Search { saved: None, keys: Vec::new(), power: 1, steps: 0 } }
    }

    fn clock_progress(probe: &mut Option<Self>, last: u64, stats: &mut super::Stats) -> bool {
        let Some(p) = probe.as_mut() else { return false; };
        if last == *p.last.get_or_insert(last) {
            return false;
        }
        *probe = None;
        stats.spin_false += 1;
        stats.spin_clock_progress += 1;
        true
    }
}

enum Phase {
    /// Brent's cycle search. `saved` is the reference state, taken at the
    /// landing `keys[0]`; `keys` holds every landing since. The reference
    /// moves to the current landing whenever `keys` reaches `power`
    /// landings, and `power` doubles. A cycle of `n` landings is found once
    /// the reference is in it and `power >= n`, wherever the search began.
    Search { saved: Option<Box<Saved>>, keys: Vec<Key>, power: usize, steps: usize },
    /// A proven cycle: step to its canonical landing, `target`.
    Canon { target: Key, left: usize },
}

impl PvClock {
    /// `watchdog`: host CPU time with no exit before a run is checked for a
    /// spin. Performance only.
    pub fn new(watchdog: Duration) -> Result<Self, String> {
        Ok(Self {
            _watchdog: Watchdog::new(watchdog)?,
            progressed: true,
            skip: 0,
            backoff: 0,
            syscalls0: 0,
            remaining: 0,
            syscall_ns: SYSCALL_NS,
            probe: None,
            landed: false,
            max_false_alarms: u64::MAX,
        })
    }

    /// Probes finish inside run_until; no spin-search reference survives a
    /// committed scheduler boundary. Page fields and syscall cost live in Machine.
    pub fn checkpoint_boundary(&self) -> Result<(), String> {
        if self.probe.is_some() {return Err("PV checkpoint inside a spin probe".into());}
        Ok(())
    }

    /// Scratch for the next KVM_RUN is overwritten by prepare/start. Watchdog
    /// phase and backoff are host performance state, recreated after restore.
    pub fn restore_boundary(&mut self) -> Result<(), String> {
        self.probe=None;self.landed=false;self.progressed=true;
        self.skip=0;self.backoff=0;self.syscalls0=0;self.remaining=0;
        self.syscall_ns=SYSCALL_NS;
        self._watchdog.hold(false)
    }

    /// One probe step at a landing. `None`: the check gave up, run freely.
    fn probe_step(&mut self, m: &mut Machine) -> Result<Option<Prepared>, String> {
        let regs = unsafe { m.vcpu.get_kvm_run().s.regs.regs };
        let key = key(&regs);
        match self.probe.as_ref().unwrap().phase {
            Phase::Search { .. } => {
                if Probe::clock_progress(&mut self.probe, m.page_get(1), &mut m.stats) {
                    return Ok(None);
                }
            }
            Phase::Canon { .. } => {}
        }
        let mut probe = self.probe.take().unwrap();
        let next = match probe.phase {
            Phase::Search { mut saved, mut keys, power, steps } => {
                if steps >= PROBE_STEPS {
                    m.stats.spin_false += 1;
                    return Ok(None);
                }
                let steps = steps + 1;
                // Equal keys are necessary for equal states; compare the
                // whole state only then.
                let back = match saved.as_mut() {
                    Some(s) if key == keys[0] => s.same(m)?,
                    _ => false,
                };
                if back {
                    // The guest is back in the reference state: it repeats
                    // this cycle forever, and `keys` is one pass of it, with
                    // its least period (the first return). Its landings are
                    // the same on every pass, so the least one is a point we
                    // reach whichever pass we noticed on.
                    let target = *keys.iter().min().unwrap();
                    if keys.iter().filter(|&&k| k == target).count() > 1 {
                        return Err(format!(
                            "{}: spin cycle at rip {:#x} has no unique canonical point (equal registers at two points of it)",
                            m.name, target.0
                        ));
                    }
                    if key == target {
                        return Ok(Some(Prepared::Idle));
                    }
                    Phase::Canon { target, left: keys.len() }
                } else if saved.is_none() || keys.len() >= power {
                    let power = if saved.is_none() { 1 } else { power * 2 };
                    Phase::Search { saved: Some(Box::new(Saved::take(m)?)), keys: vec![key], power, steps }
                } else {
                    keys.push(key);
                    Phase::Search { saved, keys, power, steps }
                }
            }
            Phase::Canon { target, left } => {
                if key == target {
                    return Ok(Some(Prepared::Idle));
                }
                if left == 0 {
                    return Err(format!("{}: lost the spin cycle at rip {:#x}", m.name, target.0));
                }
                Phase::Canon { target, left: left - 1 }
            }
        };
        if m.arm_step()? {
            // The next landing is a cap on straight-line code, so where it
            // falls depends on where stepping began: no canonical point.
            m.stats.spin_false += 1;
            return Ok(None);
        }
        probe.phase = next;
        self.probe = Some(probe);
        Ok(Some(Prepared::Run))
    }

    /// The work of `prepare` (see there).
    fn plan(&mut self, m: &mut Machine, remaining: u64, soft: u64) -> Result<Prepared, String> {
        self.remaining = remaining;
        self.syscall_ns = m.syscall_ns;
        if self.probe.is_some() {
            let step = if std::mem::take(&mut self.landed) {
                self.probe_step(m)?
            } else {
                // Some other signal interrupted the step. Where the vCPU
                // stopped may be a landing it did not report (see `prepare`),
                // so give the check up: the next one starts afresh.
                self.probe = None;
                m.stats.spin_false += 1;
                None
            };
            match step {
                Some(Prepared::Idle) => {
                    self.probe = None;
                    return Ok(Prepared::Idle);
                }
                Some(p) => return Ok(p),
                None => {
                    if m.stats.spin_false >= self.max_false_alarms {
                        return Err(format!(
                            "{}: {} spin checks found no cycle; first differences: {:?}",
                            m.name, m.stats.spin_false, m.stats.spin_mismatch
                        ));
                    }
                    self.backoff = (self.backoff * 2).clamp(1, 1 << 10);
                    self.skip = self.backoff;
                }
            }
        }
        m.set_stepping(false)?;
        // Exit when the guest clock would pass the soft stop, or at the
        // syscall whose cost reaches it.
        let syscalls = m.page_get(PAGE_SYSCALLS);
        // Also runs after non-legal watchdog/probe exits: only read the word
        // to lower the bound. Pickup, expiry, IRQ0 and RNG draws belong to
        // the machine's deterministic exit/boundary path, never to planning.
        let timer = m.page_timer.enabled.then(|| (m.page_get(PAGE_TIMER_DEADLINE), m.page_timer.seen));
        let (bound, target) = clock_stop(m.time, syscalls, self.syscall_ns, soft, timer);
        m.page_set(PAGE_BOUND, bound);
        m.page_set(PAGE_SYSCALL_TARGET, target);
        Ok(Prepared::Run)
    }
}

/// Reproduce a running guest's earlier arm after any watchdog split. Syscall
/// charges move t0 and s0 together, leaving the absolute target unchanged.
fn clock_stop(t0: u64, s0: u64, ns: u64, mut soft: u64, timer: Option<(u64, u64)>) -> (u64, u64) {
    if let Some((word, seen)) = timer {
        if word != seen && word < soft { soft = word; }
    }
    let left = soft.saturating_sub(t0).div_ceil(ns).max(1);
    (soft, s0 + left)
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
        self._watchdog.hold(self.probe.is_some())?;
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
        if run.interrupted {
            // A watchdog tick. Two in a row with no exit between: check.
            if self.skip > 0 {
                self.skip -= 1;
            } else if self.probe.is_none() && !std::mem::take(&mut self.progressed) {
                j.stats.spin_probes += 1;
                self.probe = Some(Probe::new());
                self.landed = true;
            }
            return Ok(Verdict::Advance { legal: false });
        }
        if run.debug {
            if self.probe.is_none() {
                return Err(format!("{}: debug exit outside a spin check", j.name));
            }
            self.landed = true;
            return Ok(Verdict::Advance { legal: false });
        }
        self.progressed = true;
        self.backoff = 0;
        self.skip = 0;
        if self.probe.take().is_some() {
            j.stats.spin_false += 1;
        }
        Ok(Verdict::Advance { legal: true })
    }
}

/// The machine state a cycle check compares.
struct Saved {
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

    /// Whether the machine is back in this state; if not, the first part
    /// that differs is counted in the machine's stats.
    fn same(&mut self, m: &mut Machine) -> Result<bool, String> {
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
        match part {
            None => Ok(true),
            Some(part) => {
                *m.stats.spin_mismatch.entry(part).or_default() += 1;
                Ok(false)
            }
        }
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

    /// A saved search reference needs no live vCPU for clock-progress checks.
    fn search() -> Phase {
        let saved = Saved {
            cpu: VcpuState::new(kvm_bindings::Msrs::from_entries(&[]).unwrap()),
            page: Box::new([0; 4096]),
            ram: None,
            written: Vec::new(),
        };
        Phase::Search { saved: Some(Box::new(saved)), keys: vec![(0x100000, [0; 17])], power: 2, steps: 2 }
    }

    #[test]
    fn clock_progress_abandons_search_before_the_step_limit() {
        for phase in [Phase::Search { saved: None, keys: Vec::new(), power: 1, steps: 1 }, search()] {
            let mut stats = super::super::Stats::default();
            let mut probe = Some(Probe::new());
            assert!(!Probe::clock_progress(&mut probe, 100, &mut stats));
            probe.as_mut().unwrap().phase = phase;
            assert!(!Probe::clock_progress(&mut probe, 100, &mut stats), "an unchanged clock must allow probing");
            assert_eq!((stats.spin_false, stats.spin_clock_progress), (0, 0));
            assert!(Probe::clock_progress(&mut probe, 200, &mut stats), "a clock read must abandon even an incomplete candidate");
            assert!(probe.is_none());
            assert_eq!((stats.spin_false, stats.spin_clock_progress), (1, 1));
            assert!(!Probe::clock_progress(&mut probe, 200, &mut stats));
            assert_eq!((stats.spin_false, stats.spin_clock_progress), (1, 1), "count each abandoned probe once");
        }
    }

    #[test]
    fn clock_baseline_survives_candidate_changes_and_resets_for_a_new_probe() {
        let mut stats = super::super::Stats::default();
        let mut probe = Some(Probe::new());
        assert!(!Probe::clock_progress(&mut probe, 200, &mut stats));
        probe.as_mut().unwrap().phase = search();
        assert!(!Probe::clock_progress(&mut probe, 200, &mut stats));
        probe.as_mut().unwrap().phase = Phase::Search { saved: None, keys: Vec::new(), power: 4, steps: 4 };
        assert!(!Probe::clock_progress(&mut probe, 200, &mut stats));
        assert_eq!(probe.as_ref().unwrap().last, Some(200));
        assert!(Probe::clock_progress(&mut probe, 100, &mut stats), "any difference from the start must give up");
        probe = Some(Probe::new());
        assert!(!Probe::clock_progress(&mut probe, 100, &mut stats), "a later probe captures its own baseline");
        assert_eq!((stats.spin_false, stats.spin_clock_progress), (1, 1));
    }

    // Mirror seedbox_set_next_event: only an earlier deadline changes the
    // published bound/target; C rounds up with DIV_ROUND_UP_ULL and max(1).
    fn guest_arm(t0: u64, s0: u64, ns: u64, deadline: u64, bound: u64, target: u64) -> (u64, u64) {
        if deadline >= bound { return (bound, target); }
        let left = deadline.saturating_sub(t0);
        let rounded = (left / ns) + u64::from(left % ns != 0);
        (deadline, s0 + rounded.max(1))
    }

    #[test]
    fn guest_arm_and_plan_compute_identical_bounds_and_targets() {
        for t0 in [0, 1, 999, 1000000, u64::MAX - 10000] {
            for s0 in [0, 1, 123456] {
                for ns in [1, 900, 937, 1000, 1100] {
                    let mut deadlines = vec![0, t0.saturating_sub(1), t0];
                    for offset in [1, ns - 1, ns, ns + 1, 2 * ns - 1, 2 * ns, 2 * ns + 1] {
                        deadlines.push(t0 + offset);
                    }
                    for deadline in deadlines {
                        for soft in [t0, deadline.saturating_sub(1), deadline, deadline + 1, t0 + 5000] {
                            let (bound, target) = clock_stop(t0, s0, ns, soft, None);
                            let guest = guest_arm(t0, s0, ns, deadline, bound, target);
                            let plan = clock_stop(t0, s0, ns, soft, Some((deadline, u64::MAX)));
                            assert_eq!(guest, plan, "t0={t0} s0={s0} ns={ns} deadline={deadline} soft={soft}");
                            assert_eq!(clock_stop(t0, s0, ns, soft, Some((deadline, deadline))), (bound, target), "an adopted word must not change the plan");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_unadopted_page_arm_is_split_invariant() {
        for t0 in [0, 987654] {
            for s0 in [0, 54321] {
                for ns in [900, 937, 1000, 1100] {
                    for offset in [ns - 1, ns, ns + 1, 16 * ns - 1, 16 * ns, 16 * ns + 1] {
                        let deadline = t0 + offset;
                        let soft = deadline + 1000000;
                        let want = clock_stop(t0, s0, ns, soft, Some((deadline, u64::MAX)));
                        let initial = clock_stop(t0, s0, ns, soft, None);
                        assert_eq!(guest_arm(t0, s0, ns, deadline, initial.0, initial.1), want);
                        // Every possible split before the guest's budget exit.
                        for charged in 0..(want.1 - s0) {
                            let t1 = t0 + charged * ns;
                            let s1 = s0 + charged;
                            assert_eq!(clock_stop(t1, s1, ns, soft, Some((deadline, u64::MAX))), want);
                            let initial = clock_stop(t1, s1, ns, soft, None);
                            assert_eq!(guest_arm(t1, s1, ns, deadline, initial.0, initial.1), want);
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "requires KVM"]
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
            kernel: &kernel, initrd: &[], cmdline: String::new(), mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32], mac: None, control_channel: false, disk: None, undo_margin: undo, barrier: false,
            pv_clock: true, page_timer: false,
        };
        let mut m = Machine::new(kvm, "pv-probe", &spec).unwrap();
        let mut r = m.vcpu.get_regs().unwrap();
        r.rflags = 0x202;
        m.vcpu.set_regs(&r).unwrap();
        m.pic.set_masks(0);
        m
    }

    #[test]
    #[ignore = "requires KVM"]
    fn a_page_arm_lowers_the_running_guest_clock_bound() {
        let kvm = Kvm::new().unwrap();
        let mut code = Vec::new();
        // Store a 20 us deadline and lower bound while KVM is running.
        for (address, value) in [(0xd0000070u32, 20000u32), (0xd0000074, 0), (0xd0000038, 20000)] {
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
            let stop = m.run_until(&mut clock, 1000000, 0, &mut Vec::new()).unwrap();
            assert_eq!((stop, m.time), (Stop::Halted, 20000), "watchdog={watchdog:?}");
            assert_eq!(m.pit.deadline, None);
            m.pic.set_masks(0);
            assert_eq!(m.pic.pending(), Some(0));
        }
    }

    #[test]
    #[ignore = "requires Linux x86-64 KVM"]
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
        // Simulate watchdog landings before/after an old arm is overdue.
        // An unadopted earlier arm may only lower the published bound.
        for time in [0, 9999, 10000, 10293] {
            m.time = time;
            for word in [5000, 20000, super::super::TIMER_DISARMED] {
                m.page_set(PAGE_TIMER_DEADLINE, word);
                assert!(matches!(clock.prepare(&mut m, 100000, 100000).unwrap(), Prepared::Run));
                assert_eq!(m.page_get(PAGE_BOUND), word.min(100000));
                assert_eq!(m.page_timer.seen, super::super::TIMER_DISARMED);
                assert_eq!(m.pit.deadline, Some(10000));
                assert_eq!(m.pic.pending(), None);
                assert_eq!(m.timer_rng.as_ref().unwrap().counter(), 0);
                assert_eq!(m.next_deadline(), 10000, "idle wake must ignore the raw word");
            }
        }
    }

    #[test]
    #[ignore = "requires Linux x86-64 KVM"]
    fn store_deadline_then_halt_has_the_same_wake_irq_and_witness_across_watchdogs() {
        let kvm = Kvm::new().unwrap();
        for (spin, old, word) in [(false, None, 900000u64), (false, Some(20000), 900000),
                                  (false, None, 20000), (false, Some(20000), super::super::TIMER_DISARMED),
                                  (true, None, 900000)] {
            let mut code = Vec::new();
            // Clock reads have advanced last to 30 us. Store the deadline,
            // compute for long enough to encounter watchdog splits, then HLT.
            for (address, value) in [(0xd0000008u32, 30000u32), (0xd000000c, 0),
                                     (0xd0000070, word as u32), (0xd0000074, (word >> 32) as u32)] {
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
            for (watchdog, undo) in [(if spin { Duration::from_millis(2) } else { Duration::from_secs(10) }, 0),
                                      (Duration::from_micros(100), 0),
                                      (Duration::from_micros(200), 256)] {
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
                assert_eq!(m.run_until(&mut clock, 1000000, 0, &mut Vec::new()).unwrap(), Stop::Halted);
                assert_eq!(m.time, 30000, "idle must include page-clock progress");
                assert_eq!(m.stats.spin_idles, u64::from(spin));
                if watchdog < Duration::from_secs(1) {
                    assert!(m.stats.eintr > 0, "the short watchdog must actually interrupt the guest");
                }
                m.pic.set_masks(0);
                let pending = old.is_some() || word <= 30000;
                assert_eq!(m.pic.pending(), pending.then_some(0), "overdue arms must survive replacement/disarm");
                let adopted = m.next_deadline();
                let wake = if pending { m.time } else { adopted };
                if !pending {
                    assert!((word..=word + 1000).contains(&wake));
                }
                // Model the world's halted key and wake, then expire and inject
                // once without entering the guest's absent interrupt handler.
                m.wake(wake);
                m.expire_timers().unwrap();
                m.inject();
                assert_eq!(m.stats.irqs, 1);
                assert_eq!(m.pic.pending(), None);
                let mut rng = m.timer_rng.as_ref().unwrap().clone();
                let got = (wake, adopted, m.stats.irqs, m.exit_witness(), rng.next());
                if let Some(want) = &want { assert_eq!(&got, want, "watchdog={watchdog:?} undo={undo}"); }
                else { want = Some(got); }
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
