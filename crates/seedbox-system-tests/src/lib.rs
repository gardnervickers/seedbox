//! Cross-process admission for tests that cannot replace a native hypervisor.
//!
//! Acquire before creating any VM or server, and keep the lease until every
//! owned worker has stopped. Ordinary tests use private resources and no lease.
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::{Path, PathBuf},
};

/// An exclusive kernel-resource lease shared by concurrent test binaries.
/// The caller supplies the directory, so lock tests need no global fixture.
pub struct KernelLease(File);

impl KernelLease {
    /// Stable across checkouts and Nix shells. Runners forward this path when
    /// changing user with sudo, so both processes contend for the same file.
    pub fn directory() -> PathBuf {
        std::env::var_os("SEEDBOX_TEST_RESOURCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // TMPDIR is private to each Nix shell and cannot name this lock.
                // SAFETY: geteuid has no preconditions.
                Path::new("/tmp").join(format!("seedbox-system-tests-{}", unsafe {
                    libc::geteuid()
                }))
            })
    }

    pub fn acquire(directory: &Path) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("hypervisor.lock"))?;
        loop {
            // SAFETY: the owned descriptor stays open for the whole lease.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for KernelLease {
    fn drop(&mut self) {
        // SAFETY: the descriptor is still owned; closing also releases the lock.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
