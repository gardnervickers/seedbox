//! Process-shared accounting for intentional host waits. This is transport
//! state, never guest state or retained deterministic evidence.
//!
//! One writer publishes an atomic word. Even words encode completed wait time;
//! odd words encode the monotonic-clock origin minus completed waits. A reader
//! can therefore include a currently open wait without polling the worker.

use std::{
    fs::{File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub const FILE_NAME: &str = "host-wait-clock";
const LENGTH: usize = std::mem::size_of::<AtomicU64>();

/// Host budget timestamps use the same clock as wait accounting on every platform.
pub struct Instant(u64);

impl Instant {
    pub fn now() -> io::Result<Self> {
        Ok(Self(now_ns()?))
    }
    pub fn elapsed(&self) -> io::Result<Duration> {
        now_ns()?
            .checked_sub(self.0)
            .map(Duration::from_nanos)
            .ok_or_else(|| io::Error::other("host monotonic clock moved backwards"))
    }
}

pub struct Reader(Mapping);
pub struct Writer(Mapping);

struct Mapping(NonNull<AtomicU64>);
// SAFETY: mapping lifetime is owned, and all accesses to the shared word are
// atomic. Native supported platforms provide lock-free, process-shared u64s.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn open(file: File, writable: bool) -> io::Result<Self> {
        if file.metadata()?.len() != LENGTH as u64 {
            return Err(io::Error::other("invalid host wait clock size"));
        }
        // SAFETY: map a validated, nonempty file. The owner never truncates it;
        // the private endpoint directory controls access. mmap is page-aligned.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LENGTH,
                libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 },
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(pointer) = NonNull::new(address.cast()) else {
            // SAFETY: even a mapping at address zero must be released.
            unsafe {
                libc::munmap(address, LENGTH);
            }
            return Err(io::Error::other("null host clock mapping"));
        };
        Ok(Self(pointer))
    }

    fn word(&self) -> &AtomicU64 {
        // SAFETY: mmap is aligned, remains mapped for self, and contains one
        // initialized u64. Atomic accesses are the only accesses after creation.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this is the exact address and length owned by this mapping.
        unsafe {
            libc::munmap(self.0.as_ptr().cast(), LENGTH);
        }
    }
}

fn now_ns() -> io::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid output pointer and a supported monotonic clock ID.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let nanos = u64::try_from(time.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| {
            u64::try_from(time.tv_nsec)
                .ok()
                .and_then(|ns| seconds.checked_add(ns))
        })
        .filter(|ns| *ns <= u64::MAX / 2)
        .ok_or_else(|| io::Error::other("host monotonic clock exceeds accounting range"))?;
    Ok(nanos)
}

impl Reader {
    /// Create an initially zero clock before launching its sole worker writer.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.set_len(LENGTH as u64)?;
        Ok(Self(Mapping::open(file, false)?))
    }

    pub fn waited(&self) -> io::Result<Duration> {
        // Sample the word first: a newly opened wait cannot start after the
        // timestamp sampled below. One atomic value prevents torn transitions.
        let word = self.0.word().load(Ordering::Acquire);
        let nanos = if word & 1 == 0 {
            word >> 1
        } else {
            now_ns()?
                .checked_sub(word >> 1)
                .ok_or_else(|| io::Error::other("host wait clock is ahead of monotonic time"))?
        };
        Ok(Duration::from_nanos(nanos))
    }
}

impl Writer {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self(Mapping::open(file, true)?))
    }

    pub fn begin_wait(&mut self) -> io::Result<()> {
        let word = self.0.word().load(Ordering::Acquire);
        if word & 1 != 0 {
            return Err(io::Error::other("host wait already open"));
        }
        let origin = now_ns()?
            .checked_sub(word >> 1)
            .ok_or_else(|| io::Error::other("host wait accounting exceeds uptime"))?;
        self.0.word().store((origin << 1) | 1, Ordering::Release);
        Ok(())
    }

    pub fn end_wait(&mut self) -> io::Result<()> {
        let word = self.0.word().load(Ordering::Acquire);
        if word & 1 == 0 {
            return Err(io::Error::other("host wait is not open"));
        }
        let waited = now_ns()?
            .checked_sub(word >> 1)
            .ok_or_else(|| io::Error::other("host wait clock moved backwards"))?;
        self.0.word().store(waited << 1, Ordering::Release);
        Ok(())
    }
}
