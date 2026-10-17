//! Read-only observation of live processes holding this fixture's ownership
//! leases. Saved PIDs never authorize signals or cleanup; only the real monitor
//! receipt and lease barriers do that. pidfds prevent PID-reuse ambiguity.
use seedbox_application::process_lifetime::{
    ANCHOR_ENTRY, EXECUTION_LEASE_FILE, MONITOR_ENTRY, SESSION_LEASE_FILE,
};
use std::{
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};

pub struct OwnedProcesses(Vec<(String, OwnedFd)>);
impl OwnedProcesses {
    pub fn capture(session: &Path) -> Self {
        let session = session.canonicalize().unwrap();
        let mut processes = Vec::new();
        for entry in fs::read_dir("/proc").unwrap().filter_map(Result::ok) {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == std::process::id() {
                continue;
            }
            let Ok(descriptors) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            let held = descriptors.filter_map(Result::ok).any(|fd| {
                fs::read_link(fd.path()).is_ok_and(|path| {
                    path.starts_with(&session)
                        && path.file_name().is_some_and(|name| {
                            name == EXECUTION_LEASE_FILE || name == SESSION_LEASE_FILE
                        })
                })
            });
            if !held {
                continue;
            }
            let Ok(arguments) = fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let has = |name: &str| {
                arguments
                    .split(|byte| *byte == 0)
                    .any(|arg| arg == name.as_bytes())
            };
            let role = if has(MONITOR_ENTRY) {
                "monitor"
            } else if has(ANCHOR_ENTRY) {
                "anchor"
            } else if has("driver-world-v1") {
                "driver worker"
            } else {
                "owned descendant"
            };
            // SAFETY: pidfd_open produces a fresh owned descriptor. A vanished
            // short-lived descendant may be skipped; the three required live
            // ownership roles below must still all be observed.
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if descriptor < 0 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
                continue;
            }
            eprintln!("forced-failure live ownership role={role} pid={pid}");
            processes.push((role.to_owned(), unsafe {
                OwnedFd::from_raw_fd(descriptor as i32)
            }));
        }
        for required in ["monitor", "anchor", "driver worker"] {
            assert!(
                processes.iter().any(|(role, _)| role == required),
                "missing live {required} before forced failure"
            );
        }
        Self(processes)
    }
    pub fn assert_exited(&self) {
        for (role, descriptor) in &self.0 {
            let mut event = libc::pollfd {
                fd: descriptor.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = loop {
                let result = unsafe { libc::poll(&mut event, 1, 0) };
                if result < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break result;
            };
            assert_eq!(result, 1, "{role} still live after joined cleanup");
            assert_ne!(event.revents & libc::POLLIN, 0, "{role} did not exit");
        }
    }
}
