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

use super::{pv_plan::charge, Machine};
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
        debug_assert!(!legal || m.pv_debt_ns == 0);
        let (time, seeded, throttle, timer) = (
            m.time,
            m.syscall_ns,
            m.throttle_permille,
            m.page_timer.enabled,
        );
        self.syscall_ns = super::time_page::prepare(m, time, seeded, throttle, soft, timer, legal);
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
    use super::*;
    #[test]
    fn throttled_cost_scales_the_seeded_speed_and_rounds_up() {
        assert_eq!(throttled_ns(937, 1000), 937);
        assert_eq!(throttled_ns(937, 2500), 2343);
        assert_eq!(throttled_ns(1100, 1_000_000), 1_100_000);
    }
}
