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
