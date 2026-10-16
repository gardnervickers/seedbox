//! Explicit process-scoped lease installed by the private execution-worker entry.
//! A worker hosts one execution. Parent descriptors are always close-on-exec;
//! owned subprocesses receive a descriptor only through their own spawn hook.

use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::Command,
    sync::OnceLock,
};

pub const ENTRY: &str = "--seedbox-execution-lease-v1";
pub const CHILD_FD: i32 = 198;
static LEASE: OnceLock<File> = OnceLock::new();

/// Consume the private entry prefix before parsing any other worker arguments.
/// The prefix is only supplied by the monitor or an owned nested-worker spawn.
/// Call once, before creating threads. Ordinary library callers do not install
/// process-wide execution ownership.
pub fn worker_arguments(
    mut arguments: Vec<std::ffi::OsString>,
) -> io::Result<Vec<std::ffi::OsString>> {
    if arguments.first().is_some_and(|arg| arg == ENTRY) {
        arguments.remove(0);
        if LEASE.get().is_some() {
            return Err(io::Error::other("execution lease already installed"));
        }
        if unsafe { libc::fcntl(CHILD_FD, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let lease = unsafe { File::from_raw_fd(CHILD_FD) };
        if unsafe { libc::fcntl(CHILD_FD, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        LEASE
            .set(lease)
            .map_err(|_| io::Error::other("execution lease already installed"))?;
    }
    Ok(arguments)
}

/// Attach the installed execution lease to this particular child. The return
/// value says whether CHILD_FD will exist in that child. The caller must retain
/// it for the child's entire lifetime, including error/setup and restore paths.
/// No descriptor flags in the multithreaded parent are changed.
pub fn inherit(command: &mut Command) -> io::Result<bool> {
    let Some(lease) = LEASE.get() else {
        return Ok(false);
    };
    // Do not collide with reserved destinations used by another child hook.
    let fd = unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let source = unsafe { File::from_raw_fd(fd) };
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source.as_raw_fd(), CHILD_FD) < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    Ok(true)
}
