//! Controlled Linux pre-exec probe, not a timing race or a QEMU/guest run.
use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
            net::UnixStream,
        },
    },
    process::Command,
    time::Duration,
};

struct Probe {
    pid: Option<libc::pid_t>,
    pipe: UnixStream,
}
impl Probe {
    fn reap(&mut self) -> i32 {
        let Some(pid) = self.pid.take() else {
            return 0;
        };
        let _ = self.pipe.write_all(&[1]);
        let mut status = 0;
        loop {
            let result = unsafe { libc::waitpid(pid, &mut status, 0) };
            if result == pid {
                return status;
            }
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::Interrupted
            );
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.reap();
    }
}

#[test]
fn cloexec_fork_copy_blocks_exec_after_close_and_after_temp_rename_until_child_exec() {
    let executable = CString::new(std::env::current_exe().unwrap().as_os_str().as_bytes()).unwrap();
    let help = CString::new("--help").unwrap();
    let argv = [executable.as_ptr(), help.as_ptr(), std::ptr::null()];
    for temporary in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("launcher");
        let (mut file, path) = if temporary {
            let (file, path) = tempfile::NamedTempFile::new_in(directory.path())
                .unwrap()
                .into_parts();
            (file, Some(path))
        } else {
            (File::create(&target).unwrap(), None)
        };
        file.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .unwrap();
        let inode = file.metadata().unwrap().ino();
        let fd = file.as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let (parent, child) = UnixStream::pair().unwrap();
        for pipe in [&parent, &child] {
            pipe.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            pipe.set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
        let parent_fd = parent.as_raw_fd();
        let child_fd = child.as_raw_fd();
        // The exclusive guard covers only the fork itself: the child copies
        // this test's own deliberately open writer, but no other test's
        // executable install or in-flight spawn status pipe.
        let fork_guard = seedbox_exec::writing();
        #[allow(
            clippy::disallowed_methods,
            reason = "deliberate raw fork held before exec; guarded across the fork"
        )]
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // No Rust allocation, locks or destructors after fork. Preserve a
            // writable CLOEXEC copy until this controlled child actually execs.
            unsafe {
                libc::close(parent_fd);
                let flags = libc::fcntl(fd, libc::F_GETFD);
                let access = libc::fcntl(fd, libc::F_GETFL);
                let mut ack = [u8::from(
                    flags >= 0
                        && flags & libc::FD_CLOEXEC != 0
                        && access >= 0
                        && access & libc::O_ACCMODE != libc::O_RDONLY,
                )];
                if libc::write(child_fd, ack.as_ptr().cast(), 1) != 1 {
                    libc::_exit(125);
                }
                if libc::read(child_fd, ack.as_mut_ptr().cast(), 1) != 1 {
                    libc::_exit(126);
                }
                libc::execv(executable.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        drop(child);
        let mut probe = Probe {
            pid: Some(pid),
            pipe: parent,
        };
        let mut ack = [0];
        let acked = probe.pipe.read_exact(&mut ack);
        drop(fork_guard);
        acked.unwrap();
        assert_eq!(ack, [1], "child inherited writable CLOEXEC fd");
        drop(file);
        if let Some(path) = path {
            path.persist(&target).unwrap();
        }
        assert_eq!(
            fs::metadata(&target).unwrap().ino(),
            inode,
            "rename preserves inode"
        );
        let error = crate::guarded_status(&mut Command::new(&target)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY));
        eprintln!("controlled pre-exec ETXTBSY: temporary={temporary}, fd CLOEXEC, parent closed, errno={:?}", error.raw_os_error());
        let status = probe.reap();
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(
            crate::guarded_status(&mut Command::new(&target))
                .unwrap()
                .success(),
            "same inode becomes executable after the inheriting child execs/reaps"
        );
    }
}
