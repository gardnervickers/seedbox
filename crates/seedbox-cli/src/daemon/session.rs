//! Session deletion requires ownership barriers, including after server death.
use super::*;
use seedbox_driver_execution::process_lifetime::{
    reclaim_barrier, EXECUTION_LEASE_FILE, SESSION_LEASE_FILE,
};
use std::{
    fs::File,
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

// Only this owner's shutdown waits for a transient pre-exec flock copy.
// The budget is shared across the session and every execution lease; an actual
// live owner still leaves its files intact. Startup never waits on old owners.
const CLOSE_BARRIER_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) struct SessionDirectory {
    directory: Option<tempfile::TempDir>,
    lease: Option<File>,
}
impl SessionDirectory {
    pub(super) fn path(&self) -> &Path {
        self.directory.as_ref().expect("owned session").path()
    }
    pub(super) fn lease_path(&self) -> PathBuf {
        self.path().join(SESSION_LEASE_FILE)
    }
    pub(super) fn close(self) -> std::io::Result<()> {
        self.close_with_probe(reclaim_barrier)
    }
    // Keep the same cleanup path in tests while controlling the fork/exec edge,
    // rather than guessing a sleep that will fall inside the shutdown budget.
    fn close_with_probe(
        mut self,
        probe: impl FnMut(&Path, Duration) -> std::io::Result<File>,
    ) -> std::io::Result<()> {
        let directory = self.directory.take().expect("owned session");
        self.lease.take(); // Close, never LOCK_UN a shared open description.
        match barriers(directory.path(), CLOSE_BARRIER_TIMEOUT, probe) {
            Ok(_guards) => directory.close(),
            Err(error) => {
                let _ = directory.keep();
                Err(error)
            }
        }
    }
    #[cfg(test)]
    pub(super) fn keep(mut self) -> PathBuf {
        self.lease.take();
        self.directory.take().expect("owned session").keep()
    }
}
impl Drop for SessionDirectory {
    fn drop(&mut self) {
        self.lease.take();
        if let Some(directory) = self.directory.take() {
            match barriers(directory.path(), CLOSE_BARRIER_TIMEOUT, reclaim_barrier) {
                Ok(_guards) => {
                    if let Err(error) = directory.close() {
                        eprintln!("session cleanup: {error}");
                    }
                }
                Err(error) => {
                    let path = directory.keep();
                    eprintln!("session cleanup {}: {error}; files kept", path.display());
                }
            }
        }
    }
}
/// Hold all successful independent exclusive descriptions through deletion.
/// A missing session ownership file cannot establish termination of old users.
fn barriers(
    root: &Path,
    timeout: Duration,
    mut probe: impl FnMut(&Path, Duration) -> std::io::Result<File>,
) -> std::io::Result<Vec<File>> {
    let started = Instant::now();
    let remaining = || timeout.saturating_sub(started.elapsed());
    let session = probe(&root.join(SESSION_LEASE_FILE), remaining()).map_err(|error| {
        let reason = match error.kind() {
            std::io::ErrorKind::NotFound => "session has no ownership lease; its users are unknown",
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                "session lease is held by an owner or cleanup monitor; cleanup may finish later"
            }
            _ => "session ownership could not be checked",
        };
        std::io::Error::new(error.kind(), format!("{reason}: {error}"))
    })?;
    let mut guards = vec![session];
    let mut directories = vec![root.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() && entry.file_name() == EXECUTION_LEASE_FILE {
                guards.push(probe(&entry.path(), remaining()).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "execution users remain at {}: {error}; cleanup may finish later",
                            entry.path().display()
                        ),
                    )
                })?);
            }
        }
    }
    Ok(guards)
}
/// Called only under the exclusive server lock, before accepting requests.
/// Uncertain old ownership is retained and diagnosed, never adopted. It does not
/// prevent this owner from creating a fresh unique session.
pub(super) fn session_directory(root: &Path) -> Result<SessionDirectory> {
    session_directory_with_diagnostics(root, |message| eprintln!("{message}"))
}
fn session_directory_with_diagnostics(
    root: &Path,
    mut diagnostic: impl FnMut(String),
) -> Result<SessionDirectory> {
    let sessions = root.join("sessions");
    prepare_directory(&sessions)?;
    for entry in fs::read_dir(&sessions)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("session-") {
            continue;
        }
        let path = entry.path();
        let cleanup = (|| -> std::io::Result<()> {
            if !entry.file_type()?.is_dir() {
                return Err(std::io::Error::other(
                    "session entry is not a directory; ownership is unknown",
                ));
            }
            let _guards = barriers(&path, Duration::ZERO, reclaim_barrier)?;
            fs::remove_dir_all(&path)
        })();
        if let Err(error) = cleanup {
            diagnostic(format!(
                "retained session {}: {error}; starting a fresh session; before manual removal, verify that no Seedbox processes use this path",
                path.display()
            ));
        }
    }
    let directory = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(&sessions)
        .map_err(|error| {
            Error::Setup(format!("session directory {}: {error}", sessions.display()))
        })?;
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.path().join(SESSION_LEASE_FILE))?;
    if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(SessionDirectory {
        directory: Some(directory),
        lease: Some(lease),
    })
}

// The diagnostic subprocess briefly inherits the parent's flock descriptions
// before exec closes them. Every CLI library test that spawns a subprocess
// must hold this scope through that spawn/child lifetime. Immediate session
// probes share it; unrelated tests remain parallel.
//
// This is not the exec-file write/fork ordering (seedbox_exec), which ends at
// exec: the session probes assert lease ownership across a held child's whole
// lifetime, so they need their own exclusion. Spawns here still go through
// seedbox_exec as well (bd-b02).
#[cfg(test)]
pub(super) fn test_scope() -> std::sync::MutexGuard<'static, ()> {
    static SESSION_FORK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SESSION_FORK.lock().unwrap_or_else(|poisoned| {
        // This mutex protects test fork/probe ordering, not mutable data. Keep
        // exclusion after a failed assertion without masking it behind cascades.
        eprintln!("session test scope: stateless coordination lock re-acquired");
        SESSION_FORK.clear_poison();
        poisoned.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_test::process::Guarded;
    fn fresh(root: &Path) -> (SessionDirectory, Vec<String>) {
        let mut messages = Vec::new();
        let session =
            session_directory_with_diagnostics(root, |message| messages.push(message)).unwrap();
        fs::write(session.path().join("usable"), b"new session accepts work").unwrap();
        assert_eq!(
            fs::read(session.path().join("usable")).unwrap(),
            b"new session accepts work"
        );
        (session, messages)
    }
    fn diagnosed(messages: &[String], path: &Path, reason: &str) {
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains(&path.display().to_string()),
            "{messages:?}"
        );
        assert!(messages[0].contains(reason), "{messages:?}");
        assert!(
            messages[0]
                .contains("before manual removal, verify that no Seedbox processes use this path"),
            "{messages:?}"
        );
    }
    #[test]
    fn restart_keeps_live_session_and_starts_usable_fresh_session() {
        let _scope = test_scope();
        let root = tempfile::tempdir().unwrap();
        let first = session_directory(root.path()).unwrap();
        let path = first.path().to_owned();
        fs::write(path.join("execution-data"), b"live").unwrap();
        let (second, messages) = fresh(root.path());
        diagnosed(&messages, &path, "session lease is held");
        assert_ne!(path, second.path());
        assert_eq!(fs::read(path.join("execution-data")).unwrap(), b"live");
        second.close().unwrap();
        first.keep();
        let (third, messages) = fresh(root.path());
        assert_eq!(messages, Vec::<String>::new());
        assert!(!path.exists());
        third.close().unwrap();
    }
    #[test]
    fn session_without_lease_is_preserved_and_diagnosed_without_blocking_startup() {
        let _scope = test_scope();
        let root = tempfile::tempdir().unwrap();
        prepare_directory(&root.path().join("sessions")).unwrap();
        let old = root.path().join("sessions/session-unleased");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("execution-data"), b"unknown ownership").unwrap();
        let (session, messages) = fresh(root.path());
        diagnosed(&messages, &old, "session has no ownership lease");
        assert_eq!(
            fs::read(old.join("execution-data")).unwrap(),
            b"unknown ownership"
        );
        assert!(
            !old.join(SESSION_LEASE_FILE).exists(),
            "must not fabricate ownership"
        );
        session.close().unwrap();
        assert!(old.exists());
    }
    #[test]
    fn escaped_execution_holder_preserves_shutdown_error_but_allows_fresh_startup() {
        let _scope = test_scope();
        let root = tempfile::tempdir().unwrap();
        let session = session_directory(root.path()).unwrap();
        let path = session.path().to_owned();
        let execution = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.join(EXECUTION_LEASE_FILE))
            .unwrap();
        fs::write(path.join("execution-data"), b"held").unwrap();
        assert_eq!(
            unsafe { libc::flock(execution.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        assert!(session
            .close()
            .unwrap_err()
            .to_string()
            .contains("execution users remain"));
        let (fresh_session, messages) = fresh(root.path());
        diagnosed(&messages, &path, "execution users remain");
        assert!(messages[0].contains("cleanup may finish later"));
        assert_eq!(fs::read(path.join("execution-data")).unwrap(), b"held");
        fresh_session.close().unwrap();
        drop(execution);
        let (next, messages) = fresh(root.path());
        assert_eq!(messages, Vec::<String>::new());
        assert!(!path.exists());
        next.close().unwrap();
    }
    #[test]
    fn retained_unleased_session_diagnostic_is_written_to_stderr() {
        let _scope = test_scope();
        const ROOT: &str = "SEEDBOX_SESSION_DIAGNOSTIC_TEST_ROOT";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = PathBuf::from(root);
            let session = session_directory(&root).unwrap();
            fs::write(session.path().join("usable"), b"fresh session is usable").unwrap();
            let path = session.keep();
            fs::write(
                root.join("fresh-session-path"),
                path.as_os_str().as_encoded_bytes(),
            )
            .unwrap();
            return;
        }
        let root = tempfile::tempdir().unwrap();
        prepare_directory(&root.path().join("sessions")).unwrap();
        let old = root.path().join("sessions/session-unleased");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("execution-data"), b"unknown owner").unwrap();
        let (_, module) = module_path!().split_once("::").unwrap();
        let name = format!("{module}::retained_unleased_session_diagnostic_is_written_to_stderr");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env_clear()
            .env(ROOT, root.path())
            .guarded_output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "diagnostic fixture did not run: {output:?}"
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(&old.display().to_string()), "{stderr}");
        assert!(
            stderr.contains("session has no ownership lease"),
            "{stderr}"
        );
        assert!(
            stderr
                .contains("before manual removal, verify that no Seedbox processes use this path"),
            "{stderr}"
        );
        assert_eq!(
            fs::read(old.join("execution-data")).unwrap(),
            b"unknown owner"
        );
        let bytes = fs::read(root.path().join("fresh-session-path")).unwrap();
        use std::os::unix::ffi::OsStringExt;
        let fresh = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        assert_ne!(fresh, old);
        assert_eq!(
            fs::read(fresh.join("usable")).unwrap(),
            b"fresh session is usable"
        );
    }
    use std::{
        ffi::CString,
        io::{Read, Write},
        os::unix::{ffi::OsStrExt, net::UnixStream},
    };
    // Direct fork models the documented pre-exec interval of an unrelated
    // Command::spawn. Child operations are async-signal-safe only.
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
    fn pre_exec_probe(session: &SessionDirectory) -> Probe {
        let lease = session.lease.as_ref().unwrap().as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(lease, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let executable =
            CString::new(std::env::current_exe().unwrap().as_os_str().as_bytes()).unwrap();
        let help = CString::new("--help").unwrap();
        let argv = [executable.as_ptr(), help.as_ptr(), std::ptr::null()];
        let (parent, child) = UnixStream::pair().unwrap();
        let parent_fd = parent.as_raw_fd();
        let child_fd = child.as_raw_fd();
        // Deliberate raw fork: the probe needs a child held before exec with a
        // copy of the lease. It needs private session items, so it stays here.
        #[allow(
            clippy::disallowed_methods,
            reason = "deliberate held-before-exec fork probe of the session lease (bd-31e)"
        )]
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork probe: {}", std::io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                libc::close(parent_fd);
                let flags = libc::fcntl(lease, libc::F_GETFD);
                let mut byte = [u8::from(flags >= 0 && flags & libc::FD_CLOEXEC != 0)];
                if libc::write(child_fd, byte.as_ptr().cast(), 1) != 1 {
                    libc::_exit(125);
                }
                if libc::read(child_fd, byte.as_mut_ptr().cast(), 1) != 1 {
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
        let mut inherited_cloexec = [0];
        probe.pipe.read_exact(&mut inherited_cloexec).unwrap();
        assert_eq!(
            inherited_cloexec,
            [1],
            "the inherited descriptor already has CLOEXEC"
        );
        probe
    }

    #[test]
    fn cloexec_fork_copy_temporarily_retains_lease_until_exec_not_reaper() {
        let _scope = test_scope();
        let root = tempfile::tempdir().unwrap();
        let session = session_directory(root.path()).unwrap();
        let path = session.path().to_owned();
        let mut probe = pre_exec_probe(&session);
        session.keep();
        let (fresh_session, messages) = fresh(root.path());
        eprintln!("controlled pre-exec lease diagnostic: {messages:?}");
        diagnosed(&messages, &path, "session lease is held");
        assert!(
            path.exists(),
            "unknown live ownership must remain conservative"
        );
        fresh_session.close().unwrap();
        let status = probe.reap();
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        let (after_exec, messages) = fresh(root.path());
        assert_eq!(messages, Vec::<String>::new());
        assert!(!path.exists());
        after_exec.close().unwrap();
    }

    #[test]
    fn own_close_polls_transient_fork_copies_and_keeps_one_aggregate_budget() {
        let _scope = test_scope();
        let root = tempfile::tempdir().unwrap();
        let session = session_directory(root.path()).unwrap();
        let path = session.path().to_owned();
        let execution_path = path.join(EXECUTION_LEASE_FILE);
        let execution = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&execution_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(execution.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        fs::write(path.join("execution-data"), b"keep until exec").unwrap();
        let mut child = pre_exec_probe(&session);
        drop(execution);
        let mut budgets = Vec::new();
        session
            .close_with_probe(|lease, budget| {
                // The actual production close path has dropped the parent's lease.
                // A zero-wait probe must still fail on the fork child's copy. Only
                // then let it exec; no sleep guesses the pre-exec window.
                assert!(budget <= CLOSE_BARRIER_TIMEOUT);
                if budgets.is_empty() {
                    assert!(budget > Duration::ZERO);
                    assert_eq!(lease, path.join(SESSION_LEASE_FILE));
                    assert!(reclaim_barrier(lease, Duration::ZERO).is_err());
                    assert!(reclaim_barrier(&execution_path, Duration::ZERO).is_err());
                    assert_eq!(
                        fs::read(path.join("execution-data")).unwrap(),
                        b"keep until exec"
                    );
                    let status = child.reap();
                    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
                } else {
                    assert_eq!(lease, execution_path);
                    assert!(
                        budget <= budgets[0],
                        "each lease must not restart the polling budget"
                    );
                }
                budgets.push(budget);
                reclaim_barrier(lease, budget)
            })
            .unwrap();
        assert_eq!(budgets.len(), 2);
        assert!(
            !path.exists(),
            "own close removes the session after exec closes the inherited leases"
        );
    }

    #[test]
    fn test_scope_preserves_exclusion_after_a_failed_assertion() {
        let failed = std::panic::catch_unwind(|| {
            let _scope = test_scope();
            panic!("intentional session test-scope poison probe");
        });
        assert!(failed.is_err());
        let _recovered = test_scope();
    }
}
