//! Host-only waiting hints. Publication is observed with an ordinary protocol
//! load after every hint, including a spurious hardware wake.
use super::Action;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

/// Take one spin step on the sampled position. The caller handles Sleep.
pub fn wait_on_word(action: Action, word: &AtomicU64, seen: u64) -> u64 {
    match action {
        Action::Spin => {
            for _ in 0..64 {
                let now = word.load(SeqCst);
                if now != seen {
                    return now;
                }
                std::hint::spin_loop();
            }
        }
        Action::Monitor(remaining) => monitor(word, seen, remaining),
        Action::Sleep => unreachable!("only spin actions wait on an address"),
    }
    word.load(SeqCst)
}

/// The architectural counter's frequency, on a host with timed WFE. A timed
/// wait returns even if the peer exits without writing.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn timer_hz() -> Option<u64> {
    use std::sync::OnceLock;
    static TIMER: OnceLock<Option<u64>> = OnceLock::new();
    *TIMER.get_or_init(|| {
        let mut flag: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&flag);
        // SAFETY: buffer and length match this integer sysctl.
        let timed = unsafe {
            libc::sysctlbyname(
                c"hw.optional.arm.FEAT_WFxT".as_ptr(),
                (&mut flag as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            ) == 0
        } && flag != 0;
        timed.then(|| {
            let hz: u64;
            // SAFETY: the userspace architectural counter is available on these hosts.
            unsafe {
                std::arch::asm!("mrs {}, cntfrq_el0", out(reg) hz, options(nomem, nostack));
            }
            hz
        })
    })
}

/// The timed Apple Silicon monitor is the one with a measured PV world
/// budget, so it is the only one compiled.
pub(super) fn monitor_by_default() -> bool {
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        timer_hz().is_some()
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
    {
        false
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn monitor(word: &AtomicU64, seen: u64, remaining: std::time::Duration) {
    let Some(hz) = timer_hz() else {
        return std::hint::spin_loop();
    };
    let ticks = remaining.as_nanos().min(50_000) as u64 * hz / 1_000_000_000;
    // SAFETY: the aligned atomic outlives the assembly, which only reads
    // through its address. LDAXR arms the monitor before the comparison.
    // A store between the comparison and WFET supplies a pending event.
    unsafe {
        std::arch::asm!(
            "mrs x9, cntvct_el0",
            "add x9, x9, {ticks}",
            // Re-arm after spurious events within one deadline, before
            // asking the coordinator or VM to scan its rings again.
            "3: ldaxr {tmp}, [{ptr}]",
            "cmp {tmp}, {seen}",
            "b.ne 2f",
            ".inst 0xd5031009", // WFET X9, runtime-checked FEAT_WFxT.
            "mrs {tmp}, cntvct_el0",
            "cmp {tmp}, x9",
            "b.lo 3b",
            "2: clrex",
            out("x9") _,
            tmp = out(reg) _,
            ticks = in(reg) ticks,
            ptr = in(reg) word.as_ptr(),
            seen = in(reg) seen,
            options(nostack)
        );
    }
}

/// No other host has a monitored wait that was run and measured: pause once
/// between the caller's loads.
#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
fn monitor(_: &AtomicU64, _: u64, _: std::time::Duration) {
    std::hint::spin_loop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_test_time::HangGuard;
    use std::time::Duration;

    #[test]
    fn a_monitored_wait_returns_by_its_deadline_and_a_store_is_observed() {
        // On its own thread: a wait whose deadline were broken would hang
        // inside its first call, where only an outer guard can see it.
        let (done, waited) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let word = AtomicU64::new(7);
            let monitor = Action::Monitor(Duration::from_micros(50));
            // Already written: no wait.
            assert_eq!(wait_on_word(monitor, &word, 6), 7);
            // Never written: the wait still returns.
            assert_eq!(wait_on_word(monitor, &word, 7), 7);
            // Written from another thread, at any point before or during a
            // wait: some wait, in the end, returns the new value. This shows
            // the store is observed, not that the monitor woke for it.
            std::thread::scope(|scope| {
                scope.spawn(|| word.store(8, SeqCst));
                while wait_on_word(monitor, &word, 7) == 7 {}
            });
            assert_eq!(wait_on_word(Action::Spin, &word, 7), 8);
            done.send(()).unwrap();
        });
        HangGuard::new("every monitored wait returns", Duration::from_secs(60))
            .recv(&waited)
            .unwrap();
    }
}
