//! The PV clock: no host perf counter. Compute between exits costs no
//! virtual time (the infinite-CPU model); time moves only at
//!
//! - exits the guest makes anyway (I/O, MMIO, HLT), with their usual costs,
//! - guest clock reads, `SEEDBOX_PV_READ_NS` (100 ns) each (`seedbox_rdtsc`),
//! - syscalls, counted by the guest in the time page, each costing the
//!   machine's `Machine::syscall_ns`: a fixed `SYSCALL_NS`, scaled only by
//!   an explicit throttle.
//!
//! So a run needs no counted stop: the guest kernel exits by itself when its
//! clock would pass the soft stop (the `bound` we publish) or at the syscall
//! that reaches it (`syscall_target`). Every exit is a deterministic point.
//! Application code makes progress through the guest's automatic
//! instrumentation (guest/progress): every so many executed instructions a
//! thread makes a progress call, which the kernel counts as the syscalls
//! that compute costs at the configured CPU speed (plus seeded jitter it
//! draws from a stream this VMM seeds on the time page), and the kernel's
//! syscall and `cpu_relax` hooks.
//!
//! The count is the guest's; the hard stop is ours. Syscalls counted past
//! the stop are not lost: the time they owe is a debt the machine carries
//! (`Machine::pv_debt_ns`) and the next run charges first, so a progress
//! call that straddles a stop costs what it should.
//!
//! A loop with no exit at all would never end and never let time move. A
//! watchdog (host CPU time, a performance setting) interrupts a run with no
//! exit for a while, so the VMM can report where the guest is stuck; the run
//! then resumes untouched. A watchdog landing is not a deterministic point:
//! it publishes no plan, injects nothing and adopts no guest clock progress,
//! so it changes how a stall is reported, never where the guest lands. A
//! guest that never exits stalls virtual time until the world's wall-time
//! limit, which follows from the model: such a loop waits on something only
//! virtual time could bring.

use super::pv_plan::{charge, clock_plan};
#[cfg(test)]
use super::PAGE_TIMER_DEADLINE;
use super::{Machine, PAGE_BOUND, PAGE_SYSCALLS, PAGE_SYSCALL_TARGET};
use std::time::Duration;

pub use seedbox_engine_core::cpu_charge::{throttled_ns, SYSCALL_NS};

/// Per-thread PV execution context: the watchdog timer and per-run scratch.
/// A clock may serve any machine; nothing in it is specific to one.
pub struct PvClock {
    _watchdog: Watchdog,
    period: Duration,
    syscalls0: u64,
    remaining: u64,
    syscall_ns: u64,
}

impl PvClock {
    pub fn new(watchdog: Duration) -> Result<Self, String> {
        Ok(Self {
            _watchdog: Watchdog::new(watchdog)?,
            period: watchdog,
            syscalls0: 0,
            remaining: 0,
            syscall_ns: SYSCALL_NS,
        })
    }

    /// The watchdog period: host CPU time between interruptions of a run
    /// with no exit.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Nothing host-only outlives a committed checkpoint boundary.
    pub fn checkpoint_boundary(&self) -> Result<(), String> {
        Ok(())
    }

    /// Start the per-run scratch over after restoring a machine.
    pub fn restore_boundary(&mut self) -> Result<(), String> {
        self.syscalls0 = 0;
        self.remaining = 0;
        self.syscall_ns = SYSCALL_NS;
        Ok(())
    }

    /// Before KVM_RUN, with `remaining` progress before the hard stop and the
    /// soft stop's virtual time: arrange for the run to end in time. `legal`
    /// permits publishing guest-visible plans; a watchdog resume does not.
    pub(crate) fn prepare(
        &mut self,
        m: &mut Machine,
        remaining: u64,
        soft: u64,
        legal: bool,
    ) -> Result<(), String> {
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
        // An ABI-4 arm is a sequence of guest loads/stores, not an atomic
        // transaction. Never rewrite any of its inputs at a watchdog landing:
        // even a mathematically equivalent replan can tear those loads, relax
        // a previous re-arm's bound, or move an overdue target.
        // At a legal resume the run loop has paid all outstanding progress
        // before allowing another guest instruction. Watchdog resumes retain
        // the guest's existing plan until it reaches its semantic yield.
        debug_assert!(!legal || m.pv_debt_ns == 0);
        let t0 = m.time;
        if let Some((bound, target)) = clock_plan(
            t0,
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
                m.page_set(super::PAGE_PLAN_TIME, t0);
            }
        }
        Ok(())
    }

    /// The guest's syscall count before KVM_RUN.
    pub(crate) fn start(&mut self, syscalls: u64) {
        self.syscalls0 = syscalls;
    }

    /// Progress made by the run, from the syscall count after KVM_RUN: the
    /// owed time first, then the counted syscalls, up to the hard stop. What
    /// the stop cuts off stays owed.
    pub(crate) fn read(&mut self, debt: &mut u64, syscalls: u64) -> u64 {
        let elapsed = (syscalls - self.syscalls0).saturating_mul(self.syscall_ns);
        let (charged, owed) = charge(*debt, elapsed, self.remaining);
        *debt = owed;
        charged
    }
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

impl Drop for Watchdog {
    fn drop(&mut self) {
        unsafe { libc::timer_delete(self.0) };
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
                let mut m = machine(&kvm, &code);
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
    fn machine(kvm: &Kvm, code: &[u8]) -> Machine {
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
            let mut m = machine(&kvm, &code);
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
        let mut m = machine(&kvm, &[0xf4]);
        m.page_timer_requested = true;
        m.abi = super::super::ABI_PAGE_TIMER;
        m.enable_page_timer();
        m.syscall_ns = 937;
        let mut clock = PvClock::new(Duration::from_secs(10)).unwrap();
        m.page_set(PAGE_SYSCALLS, 17);
        let mut charge = |m: &mut Machine, legal: bool| {
            clock.prepare(m, 1_000_000, 1_000_000, legal).unwrap();
            clock.start(17);
            (
                clock.read(&mut m.pv_debt_ns, 27),
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
    fn progress_debt_stops_guest_execution_and_survives_restore() {
        let kvm = Kvm::new().unwrap();
        // Count 25 us, yield, then write a byte and exit. The write must not
        // execute while any of that CPU charge is still owed.
        let code = [
            0xc7, 0x05, 0x18, 0x00, 0x00, 0xd0, 25, 0, 0, 0, // syscalls = 25
            // Force watchdog interruptions between the count and its yield.
            0xb9, 0, 0, 0, 1, 0x49, 0x75, 0xfd, 0xba, 0x2c, 0x05, 0, 0, 0x31, 0xc0,
            0xee, // yield(0)
            0xba, 0xe9, 0, 0, 0, 0xb0, b'X', 0xee, // debug console
            0xba, 0xf4, 0, 0, 0, 0x31, 0xc0, 0xee, // exit(0)
        ];
        let mut reference = None;
        for watchdog in [Duration::from_secs(10), Duration::from_micros(100)] {
            let mut m = machine(&kvm, &code);
            m.page_timer_requested = true;
            m.abi = super::super::ABI_PAGE_TIMER;
            m.enable_page_timer();
            m.pic.set_masks(0xffff);
            let mut clock = PvClock::new(watchdog).unwrap();
            let mut frames = Vec::new();
            assert_eq!(
                m.run_until(&mut clock, 10_000, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            assert_eq!((m.time, m.pv_debt_ns), (10_000, 15_000));
            assert!(m.com1.out.is_empty());
            if watchdog == Duration::from_micros(100) {
                assert!(m.stats.eintr > 0, "exercise watchdog landings with debt");
            }
            let saved = m.snapshot().unwrap();
            let mut replay = None;
            for restored in [false, true] {
                if restored {
                    m.restore(&saved).unwrap();
                    clock.restore_boundary().unwrap();
                }
                // A timer can become due while the stopped CPU pays its debt.
                m.pit.program_deadline(Some(15_000));
                m.pit.fresh = false;
                let exits = m.stats.exits;
                assert_eq!(
                    m.run_until(&mut clock, 20_000, 0, &mut frames).unwrap(),
                    Stop::Reached
                );
                assert_eq!((m.time, m.pv_debt_ns), (20_000, 5_000));
                assert_eq!(m.stats.exits, exits, "debt must drain without KVM_RUN");
                assert!(m.com1.out.is_empty());
                assert_eq!(m.pit.deadline, None);
                assert_eq!(
                    m.run_until(&mut clock, 30_000, 0, &mut frames).unwrap(),
                    Stop::Exited
                );
                assert_eq!((m.time, m.pv_debt_ns), (25_000, 0));
                assert_eq!(m.com1.out, b"X");
                let result = (m.time, m.exit_witness(), m.com1.out.clone());
                if let Some(want) = &replay {
                    assert_eq!(&result, want);
                } else {
                    replay = Some(result.clone());
                }
                if let Some(want) = &reference {
                    assert_eq!(&result, want);
                } else {
                    reference = Some(result);
                }
            }
        }
    }

    /// The guest-RAM accounting of the x86 kernel's syscall hook, as a probe:
    /// per iteration three translator-marked syscalls (a residue `pending` that
    /// counts one syscall per ten) and one 2,500 ns progress call (a `carry`
    /// below 1,000 ns), both in guest RAM like `seedbox_translator_syscalls` and
    /// `seedbox_progress_syscalls`, and a yield once the count reaches the
    /// published target. Assembled from (Intel syntax, `.code32`):
    ///
    /// ```text
    ///     mov [PENDING], 0 / mov [CARRY], 0 / mov [ITER], 0
    /// top: mov ecx, 3
    /// marked: mov eax, [PENDING]; inc eax; cmp eax, 10; jb 1f
    ///     xor eax, eax; inc [SYSCALLS]
    /// 1:  mov [PENDING], eax; YIELD_IF_DUE; dec ecx; jnz marked
    ///     mov eax, [CARRY]; add eax, 2500; xor edx, edx; mov ebx, 1000; div ebx
    ///     add [SYSCALLS], eax; mov [CARRY], edx; YIELD_IF_DUE
    ///     inc [ITER]; cmp [ITER], 400; jb top
    ///     exit(0)
    /// YIELD_IF_DUE: if TARGET.high == 0 && SYSCALLS.low >= TARGET.low: out 0x52c, 0
    /// ```
    const FRACTIONAL_GUEST: [u8; 184] = [
        0xc7, 0x05, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc7, 0x05, 0x04, 0x00, 0x20,
        0x00, 0x00, 0x00, 0x00, 0x00, 0xc7, 0x05, 0x08, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xb9, 0x03, 0x00, 0x00, 0x00, 0xa1, 0x00, 0x00, 0x20, 0x00, 0x40, 0x83, 0xf8, 0x0a, 0x72,
        0x08, 0x31, 0xc0, 0xff, 0x05, 0x18, 0x00, 0x00, 0xd0, 0xa3, 0x00, 0x00, 0x20, 0x00, 0x83,
        0x3d, 0x44, 0x00, 0x00, 0xd0, 0x00, 0x75, 0x15, 0xa1, 0x18, 0x00, 0x00, 0xd0, 0x3b, 0x05,
        0x40, 0x00, 0x00, 0xd0, 0x72, 0x08, 0xba, 0x2c, 0x05, 0x00, 0x00, 0x31, 0xc0, 0xee, 0x49,
        0x75, 0xc7, 0xa1, 0x04, 0x00, 0x20, 0x00, 0x05, 0xc4, 0x09, 0x00, 0x00, 0x31, 0xd2, 0xbb,
        0xe8, 0x03, 0x00, 0x00, 0xf7, 0xf3, 0x01, 0x05, 0x18, 0x00, 0x00, 0xd0, 0x89, 0x15, 0x04,
        0x00, 0x20, 0x00, 0x83, 0x3d, 0x44, 0x00, 0x00, 0xd0, 0x00, 0x75, 0x15, 0xa1, 0x18, 0x00,
        0x00, 0xd0, 0x3b, 0x05, 0x40, 0x00, 0x00, 0xd0, 0x72, 0x08, 0xba, 0x2c, 0x05, 0x00, 0x00,
        0x31, 0xc0, 0xee, 0xff, 0x05, 0x08, 0x00, 0x20, 0x00, 0x81, 0x3d, 0x08, 0x00, 0x20, 0x00,
        0x90, 0x01, 0x00, 0x00, 0x0f, 0x82, 0x6f, 0xff, 0xff, 0xff, 0xba, 0xf4, 0x00, 0x00, 0x00,
        0x31, 0xc0, 0xee, 0xf4,
    ];
    const PENDING: usize = 0x20_0000;
    const CARRY: usize = PENDING + 4;
    const ITERATIONS: usize = PENDING + 8;

    /// One stopped boundary: time, owed CPU time, the guest's residue, carry,
    /// iteration and the published syscall count.
    type Boundary = (u64, u64, u32, u32, u32, u64);

    fn fractional_machine(kvm: &Kvm) -> Machine {
        let mut m = machine(kvm, &FRACTIONAL_GUEST);
        m.page_timer_requested = true;
        m.abi = super::super::ABI_PAGE_TIMER;
        m.enable_page_timer();
        m.pic.set_masks(0xffff);
        m
    }

    fn boundary(m: &mut Machine) -> Boundary {
        let word =
            |memory: &[u8], at: usize| u32::from_le_bytes(memory[at..at + 4].try_into().unwrap());
        let syscalls = m.page_get(PAGE_SYSCALLS);
        let (time, debt) = (m.time, m.pv_debt_ns);
        let memory = m.memory();
        (
            time,
            debt,
            word(memory, PENDING),
            word(memory, CARRY),
            word(memory, ITERATIONS),
            syscalls,
        )
    }

    /// Run to exit through fixed 700 ns world bounds, optionally capturing a
    /// checkpoint at bound `capture` and continuing in a fresh machine.
    fn fractional_run(
        kvm: &Kvm,
        watchdog: Duration,
        capture: Option<usize>,
    ) -> (Vec<Boundary>, (u64, String)) {
        let mut m = fractional_machine(kvm);
        let mut clock = PvClock::new(watchdog).unwrap();
        let mut frames = Vec::new();
        let mut trace = Vec::new();
        for index in 1.. {
            let stop = m
                .run_until(&mut clock, index as u64 * 700, 0, &mut frames)
                .unwrap();
            trace.push(boundary(&mut m));
            if stop == Stop::Exited {
                break;
            }
            assert_eq!(stop, Stop::Reached);
            if capture == Some(index) {
                let saved = m.snapshot().unwrap();
                drop(m);
                m = fractional_machine(kvm);
                m.restore(&saved).unwrap();
                clock = PvClock::new(watchdog).unwrap();
                clock.restore_boundary().unwrap();
                assert_eq!(
                    boundary(&mut m),
                    *trace.last().unwrap(),
                    "restore at {index}"
                );
            }
            assert!(index < 10_000, "the probe never exited");
        }
        (trace, (m.time, m.exit_witness()))
    }

    /// NEXT-100 case 43: checkpoints captured with the translator-syscall
    /// residue at every value 1-9 and the progress carry at nonintegral
    /// multiples of the 1,000 ns charge must continue to the same boundaries,
    /// total charged work and stop position as the uncheckpointed run. The
    /// residue and carry live in guest RAM, so capture must neither lose nor
    /// double them.
    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn fractional_charges_survive_checkpoints_at_every_residue() {
        let kvm = Kvm::new().unwrap();
        // The work the probe charges: every whole syscall its accounting emits.
        let (mut pending, mut carry, mut syscalls) = (0u64, 0u64, 0u64);
        for _ in 0..400 {
            for _ in 0..3 {
                pending += 1;
                if pending == 10 {
                    pending = 0;
                    syscalls += 1;
                }
            }
            syscalls += (carry + 2500) / 1000;
            carry = (carry + 2500) % 1000;
        }
        for watchdog in [Duration::from_secs(10), Duration::from_micros(100)] {
            let (reference, end) = fractional_run(&kvm, watchdog, None);
            assert_eq!(end.0, syscalls * 1000, "total charged work");
            assert_eq!(reference.last().unwrap().1, 0, "no CPU time left owed");
            // Bounds whose residue covers 1-9 and whose carry is nonintegral.
            let mut captures = Vec::new();
            for residue in 1..=9 {
                let index = reference
                    .iter()
                    .position(|b| b.2 == residue && b.3 != 0 && b.4 < 400)
                    .unwrap_or_else(|| panic!("no bound with residue {residue}: {reference:?}"));
                captures.push(index + 1);
            }
            // Also where debt spans the bound.
            if let Some(index) = reference.iter().position(|b| b.1 > 0) {
                captures.push(index + 1);
            }
            for capture in captures {
                let (trace, finished) = fractional_run(&kvm, watchdog, Some(capture));
                assert_eq!(
                    trace, reference,
                    "watchdog {watchdog:?}, capture at bound {capture}"
                );
                assert_eq!(
                    finished, end,
                    "watchdog {watchdog:?}, capture at bound {capture}"
                );
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn watchdog_plans_do_not_adopt_expire_or_draw_page_timer_latency() {
        let kvm = Kvm::new().unwrap();
        let mut m = machine(&kvm, &[0xf4]);
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
                clock.prepare(&mut m, 100000, 100000, false).unwrap();
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
        for (old, word) in [
            (None, 900000u64),
            (Some(20000), 900000),
            (None, 20000),
            (Some(20000), super::super::TIMER_DISARMED),
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
            code.extend_from_slice(&[0xb9, 0, 0, 0, 0x01, 0x49, 0x75, 0xfd, 0xf4]);
            let mut want = None;
            for watchdog in [
                Duration::from_secs(10),
                Duration::from_micros(100),
                Duration::from_micros(200),
            ] {
                let mut m = machine(&kvm, &code);
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
                m.inject();
                assert_eq!(m.stats.irqs, 1);
                assert_eq!(m.pic.pending(), None);
                let mut rng = m.timer_rng.as_ref().unwrap().clone();
                let got = (wake, adopted, m.stats.irqs, m.exit_witness(), rng.next());
                if let Some(want) = &want {
                    assert_eq!(&got, want, "watchdog={watchdog:?}");
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
        for watchdog in [
            Duration::from_secs(10),
            Duration::from_micros(100),
            Duration::from_micros(700),
        ] {
            let mut m = machine(&kvm, &code);
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
                assert_eq!(&evidence, want, "watchdog={watchdog:?}");
            } else {
                want = Some(evidence);
            }
        }
    }

    /// Run to the first stop under several watchdog periods; all must agree.
    /// Returns the stop, rip, rcx and the machine of the last run.
    fn run(code: &[u8]) -> (Stop, u64, u64, Machine) {
        let kvm = Kvm::new().unwrap();
        let mut first: Option<(u64, u64, u64, String)> = None;
        let mut last = None;
        for ms in [2, 7, 3] {
            let mut clock = PvClock::new(Duration::from_millis(ms)).unwrap();
            let mut m = machine(&kvm, code);
            let stop = m
                .run_until(&mut clock, 1_000_000_000, 0, &mut Vec::new())
                .unwrap();
            let r = m.vcpu.get_regs().unwrap();
            let evidence = (m.time, r.rip, r.rcx, m.exit_witness());
            if let Some(want) = &first {
                assert_eq!(&evidence, want, "watchdog {ms} ms");
            } else {
                first = Some(evidence);
            }
            last = Some((stop, r.rip, r.rcx, m));
        }
        last.unwrap()
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn a_long_finite_loop_is_interrupted_and_left_alone() {
        // mov ecx,0x20000000; l: dec ecx; jnz l; hlt
        let (stop, rip, rcx, m) = run(&[0xb9, 0, 0, 0, 0x20, 0x49, 0x75, 0xfd, 0xf4]);
        assert_eq!(stop, Stop::Halted);
        assert_eq!((rip, rcx as u32), (0x100009, 0));
        assert_eq!(m.time, 0, "compute costs no virtual time");
        assert!(m.stats.eintr > 0, "the loop should outlast the watchdog");
    }
}
