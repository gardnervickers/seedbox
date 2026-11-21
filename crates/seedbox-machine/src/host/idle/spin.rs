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

#[cfg(target_arch = "aarch64")]
fn arm_timer() -> Option<u64> {
    use std::sync::OnceLock;
    // Timed WFE guarantees a return even if the peer exits without writing.
    // A Linux kernel advertising EVTSTRM guarantees periodic WFE events.
    static TIMER: OnceLock<Option<u64>> = OnceLock::new();
    *TIMER.get_or_init(|| {
        #[cfg(target_os = "macos")]
        let timed = {
            let mut flag: libc::c_int = 0;
            let mut len = std::mem::size_of_val(&flag);
            // SAFETY: buffer and length match this integer sysctl.
            unsafe {
                libc::sysctlbyname(
                    c"hw.optional.arm.FEAT_WFxT".as_ptr(),
                    (&mut flag as *mut libc::c_int).cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                ) == 0
                    && flag != 0
            }
        };
        #[cfg(target_os = "linux")]
        // SAFETY: HWCAP2_WFXT, from the Linux arm64 userspace ABI.
        let timed = unsafe { libc::getauxval(libc::AT_HWCAP2) } & (1 << 31) != 0;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let timed = false;
        if timed {
            let hz: u64;
            // SAFETY: the userspace architectural counter is available on these hosts.
            unsafe {
                std::arch::asm!("mrs {}, cntfrq_el0", out(reg) hz, options(nomem, nostack));
            }
            return Some(hz);
        }
        #[cfg(target_os = "linux")]
        // SAFETY: getauxval reads the process startup vector.
        if unsafe { libc::getauxval(libc::AT_HWCAP) } & (1 << 2) != 0 {
            return Some(0);
        }
        None
    })
}

/// The timed Apple Silicon monitor has a measured PV world budget.
pub(super) fn monitor_by_default() -> bool {
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        arm_timer().is_some_and(|hz| hz != 0)
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_arch = "aarch64")]
fn monitor(word: &AtomicU64, seen: u64, remaining: std::time::Duration) {
    // SAFETY: the aligned atomic outlives the assembly, which only reads
    // through its address. LDAXR arms the monitor before the comparison.
    // A store between the comparison and WFE supplies a pending event.
    unsafe {
        match arm_timer() {
            // Zero denotes a Linux kernel-provided periodic event stream.
            Some(0) => std::arch::asm!(
                "ldaxr {tmp}, [{ptr}]",
                "cmp {tmp}, {seen}",
                "b.ne 2f",
                "wfe",
                "2: clrex",
                tmp = out(reg) _,
                ptr = in(reg) word.as_ptr(),
                seen = in(reg) seen,
                options(nostack)
            ),
            Some(hz) => {
                let ticks = remaining.as_nanos().min(50_000) as u64 * hz / 1_000_000_000;
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
            None => std::hint::spin_loop(),
        }
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn monitor(_: &AtomicU64, _: u64, _: std::time::Duration) {
    std::hint::spin_loop();
}
