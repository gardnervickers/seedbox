//! Host perf counter of retired guest conditional branches (the raw event of
//! the host's counter profile), counting guest kernel and user, excluding the host.
//! Overflow signals the thread that opened the counter, so its KVM_RUN
//! returns with EINTR. Each thread counts only its own overflows: a counter on
//! another worker thread cannot disturb this one's arming.

use std::cell::Cell;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;

thread_local! {
    // Const-initialized and without Drop, so the signal handler's access
    // never runs lazy initialization.
    static OVERFLOWS: Cell<u64> = const { Cell::new(0) };
}

extern "C" fn on_overflow(_: libc::c_int) {
    OVERFLOWS.with(|o| o.set(o.get() + 1));
}

#[repr(C)]
#[derive(Default)]
struct PerfAttr {
    kind: u32,
    size: u32,
    config: u64,
    period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    rest: [u64; 10],
}

const PERF_TYPE_RAW: u32 = 4;
const FLAG_DISABLED: u64 = 1;
const FLAG_PINNED: u64 = 1 << 2;
const FLAG_EXCLUDE_HOST: u64 = 1 << 19;
const IOC_ENABLE: libc::c_ulong = 0x2400;
const IOC_PERIOD: libc::c_ulong = 0x40082404;
pub const NEVER: u64 = 1 << 62;

/// `fcntl` commands and the `F_SETOWN_EX` owner kind (linux/fcntl.h), which
/// the libc crate does not export for x86-64 glibc.
const F_SETSIG: libc::c_int = 10;
const F_SETOWN_EX: libc::c_int = 15;
const F_OWNER_TID: libc::c_int = 0;

/// The perf mmap page's fields (`struct perf_event_mmap_page`), by byte offset.
const MMAP_LOCK: usize = 8;
const MMAP_INDEX: usize = 12;
const MMAP_OFFSET: usize = 16;
const MMAP_CAPABILITIES: usize = 40;
const MMAP_PMC_WIDTH: usize = 48;
const CAP_USER_RDPMC: u64 = 1 << 2;
const MMAP_BYTES: usize = 4096;

/// `read_format`: PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING.
const READ_FORMAT: u64 = 3;

pub struct Counter {
    fd: OwnedFd,
    armed_at: Option<u64>,
    seen_overflows: u64,
    last: u64,
    /// The counter's perf mmap page, or null when it could not be mapped.
    page: *const u8,
    pub fast_reads: u64,
}

impl Counter {
    pub fn new() -> Result<Self, String> {
        let attr = PerfAttr {
            kind: PERF_TYPE_RAW,
            size: std::mem::size_of::<PerfAttr>() as u32,
            config: crate::profile::host()?.branch_event,
            period: NEVER,
            read_format: READ_FORMAT,
            flags: FLAG_DISABLED | FLAG_PINNED | FLAG_EXCLUDE_HOST,
            ..Default::default()
        };
        // SAFETY: the handler only bumps a const-initialized thread local.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_overflow as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) != 0 {
                return Err("sigaction".into());
            }
        }
        // SAFETY: `attr` is a valid perf_event_attr of the size it declares.
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 0) } as i32;
        if fd < 0 {
            return Err(format!(
                "perf_event_open: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: perf_event_open returned a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        #[repr(C)]
        struct OwnerEx {
            kind: i32,
            pid: i32,
        }
        let raw = fd.as_raw_fd();
        // SAFETY: fcntl/ioctl on our own descriptor; `owner` outlives the call.
        let setup = unsafe {
            let owner = OwnerEx {
                kind: F_OWNER_TID,
                pid: libc::syscall(libc::SYS_gettid) as i32,
            };
            libc::fcntl(raw, F_SETOWN_EX, &owner) == 0
                && libc::fcntl(raw, F_SETSIG, libc::SIGUSR1) == 0
                && libc::fcntl(raw, libc::F_SETFL, libc::O_ASYNC | libc::O_NONBLOCK) == 0
                && libc::ioctl(raw, IOC_ENABLE, 0) == 0
        };
        if !setup {
            return Err("perf counter setup".into());
        }
        // SAFETY: a fresh read-only shared mapping of the perf page; `Drop` unmaps it.
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                MMAP_BYTES,
                libc::PROT_READ,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        let page = if page == libc::MAP_FAILED {
            std::ptr::null()
        } else {
            page as *const u8
        };
        let mut c = Self {
            fd,
            armed_at: None,
            seen_overflows: 0,
            last: 0,
            page,
            fast_reads: 0,
        };
        c.read()?;
        Ok(c)
    }

    /// Cumulative guest branch count; errors if the counter was ever descheduled.
    pub fn read(&mut self) -> Result<u64, String> {
        let mut r = [0u64; 3];
        // SAFETY: reads at most 24 bytes into a 24-byte buffer.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), r.as_mut_ptr().cast(), 24) };
        if n != 24 {
            return Err(format!(
                "perf counter read: {}",
                std::io::Error::last_os_error()
            ));
        }
        if r[1] != r[2] {
            return Err(format!(
                "perf counter was multiplexed (enabled {} running {})",
                r[1], r[2]
            ));
        }
        self.last = r[0];
        Ok(r[0])
    }

    /// Read via the mmapped perf page and RDPMC (no syscall). Falls back to
    /// read() when user RDPMC is unavailable.
    pub fn read_fast(&mut self) -> Result<u64, String> {
        if self.page.is_null() {
            return self.read();
        }
        // SAFETY: `page` is the counter's live perf mmap page, read with the
        // kernel's seqlock protocol; RDPMC is permitted while cap_user_rdpmc
        // is set and the index is nonzero.
        unsafe {
            let p = self.page;
            let lock = p.add(MMAP_LOCK) as *const u32;
            let index = p.add(MMAP_INDEX) as *const u32;
            let offset = p.add(MMAP_OFFSET) as *const i64;
            let caps = p.add(MMAP_CAPABILITIES) as *const u64;
            let width = p.add(MMAP_PMC_WIDTH) as *const u16;
            loop {
                let seq = std::ptr::read_volatile(lock);
                std::sync::atomic::compiler_fence(Ordering::SeqCst);
                let idx = std::ptr::read_volatile(index);
                let cap_user_rdpmc = std::ptr::read_volatile(caps) & CAP_USER_RDPMC != 0;
                if idx == 0 || !cap_user_rdpmc {
                    return self.read();
                }
                let off = std::ptr::read_volatile(offset);
                let w = std::ptr::read_volatile(width) as u32;
                let (lo, hi): (u32, u32);
                std::arch::asm!("rdpmc", in("ecx") idx - 1, out("eax") lo, out("edx") hi, options(nostack, nomem));
                let raw = ((hi as u64) << 32 | lo as u64) as i64;
                let raw = (raw << (64 - w)) >> (64 - w);
                std::sync::atomic::compiler_fence(Ordering::SeqCst);
                if std::ptr::read_volatile(lock) == seq {
                    self.fast_reads += 1;
                    self.last = (off + raw) as u64;
                    return Ok(self.last);
                }
            }
        }
    }

    /// Last value read. Host execution never advances the counter.
    pub fn last(&self) -> u64 {
        self.last
    }

    /// Ensure an overflow fires once the count reaches `at` (absolute).
    pub fn arm(&mut self, now: u64, at: u64) -> Result<(), String> {
        let fired = OVERFLOWS.with(Cell::get);
        if self.armed_at == Some(at) && fired == self.seen_overflows {
            return Ok(());
        }
        self.set_period(at.saturating_sub(now).max(1))?;
        self.armed_at = Some(at);
        self.seen_overflows = fired;
        Ok(())
    }

    pub fn disarm(&mut self) -> Result<(), String> {
        let fired = OVERFLOWS.with(Cell::get);
        if self.armed_at == Some(u64::MAX) && fired == self.seen_overflows {
            return Ok(());
        }
        self.set_period(NEVER)?;
        self.armed_at = Some(u64::MAX);
        self.seen_overflows = fired;
        Ok(())
    }

    fn set_period(&mut self, period: u64) -> Result<(), String> {
        // SAFETY: PERF_EVENT_IOC_PERIOD reads one u64 that outlives the call.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), IOC_PERIOD, &period) };
        if rc != 0 {
            return Err(format!("perf period: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        if !self.page.is_null() {
            // SAFETY: `page` is the mapping `new` made; nothing reads it after drop.
            unsafe { libc::munmap(self.page as *mut libc::c_void, MMAP_BYTES) };
        }
        // `fd` closes itself.
    }
}
