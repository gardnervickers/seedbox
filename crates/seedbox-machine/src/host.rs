//! Host scheduling for the threads that execute machines.

pub mod doorbell;
pub mod idle;

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

/// An unnamed shared-memory object of `length` zero bytes, close-on-exec, for
/// a mapping shared with a spawned process.
#[cfg(target_os = "linux")]
pub fn shared_memory(length: usize) -> io::Result<OwnedFd> {
    // SAFETY: a static name and flags; the result is checked.
    let fd = unsafe { libc::memfd_create(c"seedbox-shm".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned a new descriptor.
    resize(unsafe { OwnedFd::from_raw_fd(fd) }, length)
}
/// macOS has no memfd: open a uniquely named object and unlink it at once.
#[cfg(target_os = "macos")]
pub fn shared_memory(length: usize) -> io::Result<OwnedFd> {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        // Short enough for macOS's 31-byte limit on these names.
        let name = std::ffi::CString::new(format!(
            "/sbx-shm.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Relaxed)
        ))
        .map_err(io::Error::other)?;
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL;
        // SAFETY: a valid name; the mode is passed as the variadic argument.
        let fd = unsafe { libc::shm_open(name.as_ptr(), flags, 0o600 as libc::c_uint) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(error);
        }
        // SAFETY: shm_open returned a new descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: a valid name; the object lives on through its descriptor.
        unsafe { libc::shm_unlink(name.as_ptr()) };
        // POSIX already makes it close-on-exec; spawns on other threads rely on that.
        // SAFETY: F_SETFD on a descriptor this function owns.
        if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        return resize(descriptor, length);
    }
}
fn resize(descriptor: OwnedFd, length: usize) -> io::Result<OwnedFd> {
    let length = libc::off_t::try_from(length).map_err(io::Error::other)?;
    // SAFETY: ftruncate on a descriptor this function owns.
    if unsafe { libc::ftruncate(descriptor.as_raw_fd(), length) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(descriptor)
}

/// How often [`IdleCores`] samples the host.
const SAMPLE: std::time::Duration = std::time::Duration::from_millis(20);

/// The host's idle cores over its last sampling interval, for a spinning
/// thread to tell whether its spin can only take a core someone else needs.
pub struct IdleCores {
    cpus: u64,
    /// The last sample: when, and the host's idle and total CPU ticks.
    last: Option<(std::time::Instant, u64, u64)>,
    idle: u64,
}
impl Default for IdleCores {
    fn default() -> Self {
        // SAFETY: a plain query.
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        Self {
            cpus: u64::try_from(cpus).unwrap_or(1).max(1),
            last: None,
            idle: 0,
        }
    }
}
impl IdleCores {
    /// Whole idle cores between the last two samples, sampling again once the
    /// last sample is [`SAMPLE`] old. Zero until two samples exist, and on a
    /// host this cannot measure.
    pub fn idle(&mut self) -> u64 {
        let now = std::time::Instant::now();
        if self
            .last
            .is_some_and(|(at, ..)| now.duration_since(at) < SAMPLE)
        {
            return self.idle;
        }
        let Some((idle, total)) = cpu_ticks() else {
            return 0;
        };
        if let Some((_, last_idle, last_total)) = self.last {
            let (idle, total) = (idle.wrapping_sub(last_idle), total.wrapping_sub(last_total));
            self.idle = idle
                .saturating_mul(self.cpus)
                .checked_div(total)
                .unwrap_or(0)
                .min(self.cpus);
        }
        self.last = Some((now, idle, total));
        self.idle
    }
}

/// The host's cumulative idle and total CPU ticks over every core.
#[cfg(target_os = "macos")]
fn cpu_ticks() -> Option<(u64, u64)> {
    use std::sync::OnceLock;
    extern "C" {
        // <mach/mach_init.h>; the libc binding is deprecated in favor of a
        // crate this workspace does not otherwise need.
        fn mach_host_self() -> libc::mach_port_t;
    }
    // One send right for the process, not one per sample.
    static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
    // SAFETY: returns the host name port; it is never deallocated.
    let host = *HOST.get_or_init(|| unsafe { mach_host_self() });
    let mut info = libc::host_cpu_load_info { cpu_ticks: [0; 4] };
    let mut count = (std::mem::size_of::<libc::host_cpu_load_info>()
        / std::mem::size_of::<libc::integer_t>())
        as libc::mach_msg_type_number_t;
    // SAFETY: `info` holds `count` integers, the size of this flavor's record.
    let result = unsafe {
        libc::host_statistics(
            host,
            libc::HOST_CPU_LOAD_INFO,
            (&mut info as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS {
        return None;
    }
    let ticks = info.cpu_ticks.map(u64::from);
    Some((ticks[libc::CPU_STATE_IDLE as usize], ticks.iter().sum()))
}
/// Unmeasured: callers keep their contended behavior.
#[cfg(not(target_os = "macos"))]
fn cpu_ticks() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_cores_never_exceed_the_host() {
        let mut cores = IdleCores::default();
        assert_eq!(cores.idle(), 0, "one sample measures nothing");
        // Force a second sample.
        cores.last = cores
            .last
            .map(|(at, idle, total)| (at - SAMPLE, idle, total));
        assert!(cores.idle() <= cores.cpus);
    }
}
