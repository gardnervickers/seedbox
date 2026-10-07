//! Host perf counter of retired guest conditional branches (the raw event of
//! the host's counter profile), counting guest kernel and user, excluding the host.
//! Overflow signals the world thread so KVM_RUN returns with EINTR.

use std::sync::atomic::{AtomicU64, Ordering};

pub static OVERFLOWS: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_overflow(_: libc::c_int) {
    OVERFLOWS.fetch_add(1, Ordering::Relaxed);
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

pub struct Counter {
    fd: i32,
    armed_at: Option<u64>,
    seen_overflows: u64,
    last: u64,
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
            read_format: 3,
            flags: FLAG_DISABLED | FLAG_PINNED | FLAG_EXCLUDE_HOST,
            ..Default::default()
        };
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_overflow as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()) != 0 {
                return Err("sigaction".into());
            }
            let fd = libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 0) as i32;
            if fd < 0 {
                return Err(format!(
                    "perf_event_open: {}",
                    std::io::Error::last_os_error()
                ));
            }
            #[repr(C)]
            struct OwnerEx {
                kind: i32,
                pid: i32,
            }
            let owner = OwnerEx {
                kind: 0,
                pid: libc::syscall(libc::SYS_gettid) as i32,
            };
            if libc::fcntl(fd, 15 /* F_SETOWN_EX */, &owner) != 0
                || libc::fcntl(fd, 10 /* F_SETSIG */, libc::SIGUSR1) != 0
                || libc::fcntl(fd, libc::F_SETFL, libc::O_ASYNC | libc::O_NONBLOCK) != 0
                || libc::ioctl(fd, IOC_ENABLE, 0) != 0
            {
                return Err("perf counter setup".into());
            }
            let page = libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
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
            c.read();
            Ok(c)
        }
    }

    /// Cumulative guest branch count; errors if the counter was ever descheduled.
    pub fn read(&mut self) -> u64 {
        let mut r = [0u64; 3];
        let n = unsafe { libc::read(self.fd, r.as_mut_ptr().cast(), 24) };
        assert_eq!(n, 24, "perf counter read");
        assert!(
            r[1] == r[2],
            "perf counter was multiplexed (enabled {} running {})",
            r[1],
            r[2]
        );
        self.last = r[0];
        r[0]
    }

    /// Read via the mmapped perf page and RDPMC (no syscall). Falls back to
    /// read() when user RDPMC is unavailable.
    pub fn read_fast(&mut self) -> u64 {
        if self.page.is_null() {
            return self.read();
        }
        unsafe {
            let p = self.page;
            let lock = p.add(8) as *const u32;
            let index = p.add(12) as *const u32;
            let offset = p.add(16) as *const i64;
            let caps = p.add(40) as *const u64;
            let width = p.add(48) as *const u16;
            loop {
                let seq = std::ptr::read_volatile(lock);
                std::sync::atomic::compiler_fence(Ordering::SeqCst);
                let idx = std::ptr::read_volatile(index);
                let cap_user_rdpmc = (std::ptr::read_volatile(caps) >> 2) & 1 == 1;
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
                    return self.last;
                }
            }
        }
    }

    /// Last value read. Host execution never advances the counter.
    pub fn last(&self) -> u64 {
        self.last
    }

    /// Ensure an overflow fires once the count reaches `at` (absolute).
    pub fn arm(&mut self, now: u64, at: u64) {
        let fired = OVERFLOWS.load(Ordering::Relaxed);
        if self.armed_at == Some(at) && fired == self.seen_overflows {
            return;
        }
        let period = at.saturating_sub(now).max(1);
        let rc = unsafe { libc::ioctl(self.fd, IOC_PERIOD, &period) };
        assert_eq!(rc, 0, "perf period");
        self.armed_at = Some(at);
        self.seen_overflows = fired;
    }

    pub fn disarm(&mut self) {
        let fired = OVERFLOWS.load(Ordering::Relaxed);
        if self.armed_at == Some(u64::MAX) && fired == self.seen_overflows {
            return;
        }
        let rc = unsafe { libc::ioctl(self.fd, IOC_PERIOD, &NEVER) };
        assert_eq!(rc, 0, "perf period");
        self.armed_at = Some(u64::MAX);
        self.seen_overflows = fired;
    }
}
