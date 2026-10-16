//! Process ownership for one execution. The monitor is the worker's sole parent.
//!
//! A successful receipt, not monitor exit alone, authorizes removal of execution
//! files. This module does not recover authority from saved process identifiers.

use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub const MONITOR_ENTRY: &str = "--seedbox-execution-monitor-v1";
pub const ANCHOR_ENTRY: &str = "--seedbox-execution-anchor-v1";
pub const WORKER_LEASE_ENTRY: &str = "--seedbox-execution-lease-v1";
const STATUS_FD: RawFd = 197;
const LEASE_FD: RawFd = 198;
const SESSION_FD: RawFd = 199;
pub const EXECUTION_LEASE_FILE: &str = "execution.lease";
pub const SESSION_LEASE_FILE: &str = "session.lease";
const TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);
const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
/// Finish covers both bounded monitor phases plus status delivery/scheduling.
pub const COMPLETION_TIMEOUT: Duration = TERMINATION_TIMEOUT
    .saturating_add(BARRIER_TIMEOUT)
    .saturating_add(Duration::from_secs(2));

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkerExit {
    Code(i32),
    Signal(i32),
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum GroupTermination {
    Signalled,
    AlreadyAbsent,
    /// Darwin can deny a group signal when its only member is a zombie.
    /// This observation alone never authorizes cleanup.
    ExitedLeaderPermissionDenied,
    Failed(i32),
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExitBarrier {
    Released,
    Failed(String),
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Completion {
    pub worker: Option<WorkerExit>,
    pub group: GroupTermination,
    pub barrier: ExitBarrier,
}
impl Completion {
    pub fn cleanup_authorized(&self) -> bool {
        self.worker.is_some()
            && !matches!(
                self.group,
                GroupTermination::Failed(_) | GroupTermination::ExitedLeaderPermissionDenied
            )
            && self.barrier == ExitBarrier::Released
    }
}

/// The server owns only this monitor, never the worker. Closing `owner` requests
/// termination. Dropping or cancelling must not kill the monitor.
pub struct OwnedExecution {
    owner: Option<UnixStream>,
    pending: Option<MonitorReceipt>,
}
struct MonitorReceipt {
    monitor: Child,
    status: UnixStream,
    completion: Option<Completion>,
    receipt: Vec<u8>,
    guards: Vec<Box<dyn Send>>,
}
impl Drop for MonitorReceipt {
    fn drop(&mut self) {
        if !self
            .completion
            .as_ref()
            .is_some_and(Completion::cleanup_authorized)
        {
            // Destructors of runtime/scratch guards may delete worker inputs.
            // Missing or failed cleanup proof must preserve those inputs.
            std::mem::forget(std::mem::take(&mut self.guards));
        }
    }
}
// Initialized before any monitor starts. Dropping an async task never waits for
// process cleanup: it transfers monitor parent/reaper ownership to this thread.
fn reaper() -> io::Result<&'static std::sync::mpsc::Sender<MonitorReceipt>> {
    static REAPER: std::sync::OnceLock<Result<std::sync::mpsc::Sender<MonitorReceipt>, String>> =
        std::sync::OnceLock::new();
    match REAPER.get_or_init(|| {
        let (send, receive) = std::sync::mpsc::channel::<MonitorReceipt>();
        std::thread::Builder::new()
            .name("execution-monitor-reaper".into())
            .spawn(move || {
                for mut pending in receive {
                    match pending.finish() {
                        Ok(completion) if !completion.cleanup_authorized() => eprintln!(
                            "execution monitor cleanup failed: {completion:?}; execution files kept"
                        ),
                        Err(error) => eprintln!("execution monitor cleanup: {error}"),
                        _ => {}
                    }
                    // The monitor remains the sole worker reaper. Never kill it.
                    let _ = pending.monitor.wait();
                }
            })
            .map(|_| send)
            .map_err(|error| error.to_string())
    }) {
        Ok(sender) => Ok(sender),
        Err(error) => Err(io::Error::other(error.clone())),
    }
}
/// Explicit ownership paths supplied by the execution/session owner. `session`
/// must name an already-created session lease. `None` means this execution has
/// no owning server session; it never requests discovery or lease fabrication.
pub struct LeasePaths<'a> {
    pub execution: &'a Path,
    pub session: Option<&'a Path>,
}
impl OwnedExecution {
    /// Start a private monitor entry in a fresh group. The supplied executable
    /// must dispatch MONITOR_ENTRY before constructing threads or runtimes.
    /// Its worker entry must consume WORKER_LEASE_ENTRY before normal dispatch.
    pub fn spawn(
        executable: &Path,
        worker_executable: &Path,
        arguments: &[std::ffi::OsString],
        environment: &[(std::ffi::OsString, std::ffi::OsString)],
        stdout: File,
        stderr: File,
        leases: LeasePaths<'_>,
    ) -> io::Result<Self> {
        reaper()?;
        use std::os::unix::ffi::OsStrExt;
        let environment: Vec<_> = environment
            .iter()
            .map(|(key, value)| (key.as_bytes(), value.as_bytes()))
            .collect();
        let environment = serde_json::to_vec(&environment)?;
        if environment.len() > 1024 * 1024 {
            return Err(io::Error::other(
                "explicit worker environment exceeds 1 MiB",
            ));
        }
        let lease = shared_lease(leases.execution, true)?;
        // The caller supplies authority explicitly. A named but missing session
        // lease is an error, not permission to create or discover another one.
        let session = match leases.session {
            Some(path) => shared_lease(path, false).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "session lease {}: {error}; session lease must already exist",
                        path.display()
                    ),
                )
            })?,
            None => lease.try_clone()?,
        };
        let session = high_descriptor(&session)?;
        let (mut owner, input) = UnixStream::pair()?;
        let (status, output) = UnixStream::pair()?;
        let output = high_descriptor(&output)?;
        let lease = high_descriptor(&lease)?;
        let mut command = Command::new(executable);
        command
            .arg(MONITOR_ENTRY)
            .arg(worker_executable)
            .args(arguments)
            .env_clear()
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(input)))
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);
        // Only async-signal-safe operations occur after fork. The parent's
        // descriptors always retain CLOEXEC, including during concurrent spawns.
        unsafe {
            command.pre_exec(move || {
                inherit(output.as_raw_fd(), STATUS_FD)?;
                inherit(lease.as_raw_fd(), LEASE_FD)?;
                inherit(session.as_raw_fd(), SESSION_FD)
            });
        }
        status.set_nonblocking(true)?;
        let monitor = command.spawn()?;
        drop(command); // No parent-side copies of child stdio/control endpoints.
        let mut execution = Self {
            owner: None,
            pending: Some(MonitorReceipt {
                monitor,
                status,
                completion: None,
                receipt: Vec::new(),
                guards: Vec::new(),
            }),
        };
        owner.set_write_timeout(Some(COMPLETION_TIMEOUT))?;
        owner.write_all(&(environment.len() as u64).to_le_bytes())?;
        owner.write_all(&environment)?;
        execution.owner = Some(owner);
        Ok(execution)
    }

    /// Retain a runtime/scratch owner through monitor join. On unproven cleanup
    /// its destructor is deliberately not run: worker inputs must remain intact.
    pub fn retain_until_join(&mut self, guard: impl Send + 'static) {
        self.pending
            .as_mut()
            .expect("owned monitor")
            .guards
            .push(Box::new(guard));
    }
    pub fn cancel(&mut self) {
        self.owner.take();
    }

    /// Observe a complete receipt without waiting for worker execution.
    /// Call from a blocking execution context: consuming the terminal receipt
    /// also joins the monitor after its final write.
    pub fn poll(&mut self) -> io::Result<Option<&Completion>> {
        self.pending.as_mut().expect("owned monitor").poll()
    }
    /// Wait for cleanup in an explicit blocking execution context, never on a
    /// Tokio executor thread. Drop only cancels and hands off the owned join.
    pub fn finish(&mut self) -> io::Result<&Completion> {
        self.pending.as_mut().expect("owned monitor").finish()
    }
}
impl MonitorReceipt {
    fn poll(&mut self) -> io::Result<Option<&Completion>> {
        if self.completion.is_some() {
            return Ok(self.completion.as_ref());
        }
        loop {
            let mut chunk = [0; 1024];
            match self.status.read(&mut chunk) {
                Ok(0) => {
                    if self.receipt.is_empty() {
                        return Err(io::Error::other(
                            "execution monitor exited without a receipt; execution files must be kept",
                        ));
                    }
                    break;
                }
                Ok(n) => {
                    self.receipt.extend_from_slice(&chunk[..n]);
                    if self.receipt.len() > 16384 {
                        return Err(io::Error::other(
                            "execution monitor status exceeds 16384 bytes",
                        ));
                    }
                    if self.receipt.last() == Some(&b'\n') {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let completion: Completion = serde_json::from_slice(&self.receipt).map_err(|e| {
            io::Error::other(format!(
                "missing or invalid execution monitor receipt: {e}; execution files must be kept"
            ))
        })?;
        self.monitor.wait()?;
        if completion.cleanup_authorized() {
            // A blocking caller publishing completion must observe guard cleanup,
            // not merely enqueue it on the async-drop reaper. Missing/failed
            // proof retains guards exactly as before.
            self.guards.clear();
        }
        self.completion = Some(completion);
        Ok(self.completion.as_ref())
    }

    /// Obtain the typed receipt and reap only the monitor. Missing/truncated
    /// status, timeout, or failure never authorizes directory removal.
    pub fn finish(&mut self) -> io::Result<&Completion> {
        let start = Instant::now();
        while self.poll()?.is_none() {
            if start.elapsed() >= COMPLETION_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "execution monitor completion timed out; execution files must be kept",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(self.completion.as_ref().expect("completed receipt"))
    }
}
impl Drop for OwnedExecution {
    fn drop(&mut self) {
        self.cancel();
        if let Some(pending) = self.pending.take() {
            // Reaper initialization preceded spawn, so ownership can always be
            // transferred without creating a thread or waiting here.
            if let Err(error) = reaper().expect("initialized before launch").send(pending) {
                eprintln!("execution monitor reaper unavailable; execution files must be kept");
                // This denotes an unexpected reaper-thread failure, not cleanup
                // success. Keep the monitor Child owned rather than kill it.
                std::mem::forget(error.0);
            }
        }
    }
}

/// Run exclusively in the private, single-threaded monitor process. It owns the
/// only worker Child and never reaps that child before the final group signal.
/// No code after dropping the monitor lease accesses a session path.
pub fn serve_monitor(mut arguments: Vec<std::ffi::OsString>) -> io::Result<()> {
    let pid = unsafe { libc::getpid() };
    if unsafe { libc::getpgrp() } != pid {
        return Err(io::Error::other(
            "execution monitor must lead its own process group",
        ));
    }
    let mut status = unsafe { File::from_raw_fd(STATUS_FD) };
    let lease = unsafe { File::from_raw_fd(LEASE_FD) };
    let session = unsafe { File::from_raw_fd(SESSION_FD) };
    close_on_exec(session.as_raw_fd())?;
    close_on_exec(status.as_raw_fd())?;
    close_on_exec(lease.as_raw_fd())?;
    // Independent open description, prepared before spawning. The pathname
    // never has to be rediscovered after the execution lease is released.
    let probe = independent_probe(&lease)?;
    let mut owner = unsafe { File::from_raw_fd(0) };
    let mut length = [0; 8];
    owner.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    if length > 1024 * 1024 {
        return Err(io::Error::other(
            "explicit worker environment exceeds 1 MiB",
        ));
    }
    let mut encoded = vec![0; length as usize];
    owner.read_exact(&mut encoded)?;
    let environment: Vec<(Vec<u8>, Vec<u8>)> = serde_json::from_slice(&encoded)?;
    use std::os::unix::ffi::OsStringExt;
    let environment: Vec<_> = environment
        .into_iter()
        .map(|(key, value)| {
            (
                std::ffi::OsString::from_vec(key),
                std::ffi::OsString::from_vec(value),
            )
        })
        .collect();
    let executable = std::env::current_exe()?;
    if arguments.is_empty() {
        return Err(io::Error::other(
            "execution monitor requires a worker executable",
        ));
    }
    let worker_executable = arguments.remove(0);
    let (anchor_owner, anchor_input) = UnixStream::pair()?;
    let mut anchor_command = Command::new(&executable);
    anchor_command
        .arg(ANCHOR_ENTRY)
        .env_clear()
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(anchor_input)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let anchor_lease = high_descriptor(&lease)?;
    unsafe {
        anchor_command.pre_exec(move || inherit(anchor_lease.as_raw_fd(), LEASE_FD));
    }
    let anchor = anchor_command.spawn()?;
    drop(anchor_command);
    let group_id = anchor.id() as i32;
    let mut children = GroupChildren {
        anchor,
        worker: None,
        armed: true,
    };
    if observe_exit(&children.anchor)?.is_some() {
        return Err(io::Error::other(
            "execution group anchor exited before worker launch; execution files must be kept",
        ));
    }
    let (worker_owner, worker_input) = UnixStream::pair()?;
    let mut command = Command::new(worker_executable);
    command
        .args(arguments)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(worker_input)))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(group_id);
    let inherited_lease = high_descriptor(&lease)?;
    unsafe {
        command.pre_exec(move || inherit(inherited_lease.as_raw_fd(), LEASE_FD));
    }
    children.worker = Some(command.spawn()?);
    drop(command);
    let worker = children.worker.as_ref().expect("spawned worker");
    // Copies of the worker's output endpoints must not keep files open or mask
    // EOF. Status has its own descriptor and is not worker stdout/stderr.
    unsafe {
        libc::close(1);
        libc::close(2);
    }
    let flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(0, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let observed = loop {
        if observe_exit(&children.anchor)?.is_some() {
            return Err(io::Error::other(
                "execution group anchor exited before termination; execution files must be kept",
            ));
        }
        if let Some(exit) = observe_exit(worker)? {
            break Some(exit);
        }
        let mut byte = [0];
        match owner.read(&mut byte) {
            Ok(0) => break None,
            Ok(_) => {}
            Err(e)
                if e.kind() == io::ErrorKind::Interrupted
                    || e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => break None,
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let group = if unsafe { libc::kill(-group_id, libc::SIGKILL) } == 0 {
        GroupTermination::Signalled
    } else {
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        match errno {
            libc::ESRCH => GroupTermination::AlreadyAbsent,
            #[cfg(target_os = "macos")]
            libc::EPERM if observed.is_some() => GroupTermination::ExitedLeaderPermissionDenied,
            _ => GroupTermination::Failed(errno),
        }
    };
    // On signal failure do not block forever waiting for a still-live worker.
    // Its shared lease keeps startup reclamation closed; report failure now.
    if matches!(
        group,
        GroupTermination::Failed(_) | GroupTermination::ExitedLeaderPermissionDenied
    ) {
        return send(
            &mut status,
            &Completion {
                worker: observed,
                group,
                barrier: ExitBarrier::Failed(
                    "worker group termination failed; execution files must be kept".into(),
                ),
            },
        );
    }
    drop(worker_owner);
    drop(anchor_owner);
    let deadline = Instant::now();
    let exit = loop {
        if let Some(exit) = observe_exit(worker)? {
            break exit;
        }
        if deadline.elapsed() >= TERMINATION_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker did not exit after group termination; execution files must be kept",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    children.worker.as_mut().expect("spawned worker").wait()?;
    // The live anchor makes the final group signal possible on Darwin even
    // when the worker is a zombie. Reap it only after that signal.
    children.anchor.wait()?;
    // Disarm Drop after both owned children have been reaped.
    children.worker = None;
    children.armed = false;
    drop(lease);
    // Every following operation uses already-open descriptors, never session paths.
    drop(session);
    let barrier = match exclusive_barrier(&probe, BARRIER_TIMEOUT) {
        Ok(()) => ExitBarrier::Released,
        Err(e) => ExitBarrier::Failed(format!(
            "execution lease still held after group termination: {e}; execution files must be kept"
        )),
    };
    send(
        &mut status,
        &Completion {
            worker: Some(exit),
            group,
            barrier,
        },
    )
}

/// Minimal live process-group leader. Only its monitor holds the input writer.
/// The monitor normally kills it with the entire group; unexpected EOF exits it.
/// This entry is called before creating threads. It never opens session paths.
pub fn serve_anchor() -> io::Result<()> {
    let _lease = unsafe { File::from_raw_fd(LEASE_FD) };
    close_on_exec(_lease.as_raw_fd())?;
    let mut input = io::stdin().lock();
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

// On launch/setup failure retain leader identity until the last group signal.
// This is not an extra monitor-death recovery mechanism.
struct GroupChildren {
    anchor: Child,
    worker: Option<Child>,
    armed: bool,
}
impl Drop for GroupChildren {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if unsafe { libc::kill(-(self.anchor.id() as i32), libc::SIGKILL) } != 0
            && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            return;
        }
        if let Some(worker) = &mut self.worker {
            let _ = worker.wait();
        }
        let _ = self.anchor.wait();
    }
}

fn send(status: &mut File, completion: &Completion) -> io::Result<()> {
    serde_json::to_writer(&mut *status, completion)?;
    status.write_all(b"\n")
}
fn observe_exit(child: &Child) -> io::Result<Option<WorkerExit>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    #[cfg(target_os = "linux")]
    let (pid, code) = unsafe { (info.si_pid(), info.si_status()) };
    #[cfg(target_os = "macos")]
    let (pid, code) = (info.si_pid, info.si_status);
    Ok((pid != 0).then_some({
        if info.si_code == libc::CLD_EXITED {
            WorkerExit::Code(code)
        } else {
            WorkerExit::Signal(code)
        }
    }))
}
fn close_on_exec(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn inherit(source: RawFd, target: RawFd) -> io::Result<()> {
    if unsafe { libc::dup2(source, target) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn high_descriptor(file: &impl AsRawFd) -> io::Result<File> {
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, SESSION_FD + 1) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn shared_lease(path: &Path, create: bool) -> io::Result<File> {
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)?;
    if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(lease)
}
fn independent_probe(file: &File) -> io::Result<File> {
    // F_GETPATH on Darwin / proc on Linux recover an existing open file, not
    // process identity. Neither implementation signals a stored PID.
    #[cfg(target_os = "linux")]
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    #[cfg(target_os = "macos")]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = vec![0u8; libc::PATH_MAX as usize];
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        bytes.truncate(bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len()));
        std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    OpenOptions::new().read(true).write(true).open(path)
}
/// Acquire an independent exclusive description. Keep the returned file locked
/// through deletion; never unlock a shared inherited open description.
pub fn reclaim_barrier(path: &Path, timeout: Duration) -> io::Result<File> {
    let probe = OpenOptions::new().read(true).write(true).open(path)?;
    exclusive_barrier(&probe, timeout)?;
    Ok(probe)
}
fn exclusive_barrier(probe: &File, timeout: Duration) -> io::Result<()> {
    let start = Instant::now();
    loop {
        if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock && error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
        if start.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "execution lease exclusive probe timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Initialize a private execution worker before dispatching its backend entry.
pub use seedbox_machine::process_lease::worker_arguments;
