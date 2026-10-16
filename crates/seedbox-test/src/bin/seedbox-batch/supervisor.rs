use super::options::{Options, Result};
#[cfg(test)]
use std::ffi::OsString;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::atomic::{AtomicI32, Ordering},
    thread,
    time::{Duration, Instant},
};

static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);
const SIGNALS: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

#[cfg(test)]
#[derive(Debug)]
pub(super) enum Trace {
    Spawn {
        pid: i32,
        session: i32,
        program: OsString,
        args: Vec<OsString>,
    },
    Wait(i32),
    /// A signal sent to an owned group.
    Signal {
        pid: i32,
        signal: i32,
    },
    /// The TERM grace ran out with the group leader still running, after
    /// this much grace-clock time.
    GraceExpired {
        pid: i32,
        waited: Duration,
    },
}
#[cfg(test)]
thread_local! { static TRACE: std::cell::RefCell<Vec<Trace>> = const { std::cell::RefCell::new(Vec::new()) }; }
#[cfg(test)]
pub(super) fn take_trace() -> Vec<Trace> {
    TRACE.with(|trace| trace.take())
}

/// Time a stopped group gets to exit after SIGTERM before SIGKILL.
const GRACE: Duration = Duration::from_secs(2);
/// How often the grace loop checks for exit.
const GRACE_POLL: Duration = Duration::from_millis(10);

/// The clock that decides when the TERM grace expires. Production measures
/// host monotonic time. Tests count it in polls instead (each still sleeps
/// for real), so expiry follows a fixed number of observed non-exits rather
/// than host speed, while children keep real time to react to SIGTERM.
struct GraceClock {
    #[cfg(not(test))]
    start: Instant,
    #[cfg(test)]
    polled: Duration,
}
impl GraceClock {
    fn start() -> Self {
        Self {
            #[cfg(not(test))]
            start: Instant::now(),
            #[cfg(test)]
            polled: Duration::ZERO,
        }
    }
    #[cfg(not(test))]
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
    #[cfg(test)]
    fn elapsed(&self) -> Duration {
        self.polled
    }
    fn poll(&mut self) {
        thread::sleep(GRACE_POLL);
        #[cfg(test)]
        {
            self.polled += GRACE_POLL;
        }
    }
}

fn sidecar(output: &Path, suffix: &str) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

pub(super) fn enable_subreaper() -> io::Result<()> {
    // Linux adopts orphaned descendants so the final group wait also reaps them.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("cannot become child subreaper: {error}"),
        ));
    }
    Ok(())
}

extern "C" fn interrupted(signum: i32) {
    PENDING_SIGNAL.store(signum, Ordering::Relaxed);
}
struct Signals(Vec<(i32, libc::sigaction)>);
impl Signals {
    fn install() -> io::Result<Self> {
        PENDING_SIGNAL.store(0, Ordering::Relaxed);
        let mut installed = Self(Vec::new());
        for signal in SIGNALS {
            let mut old = unsafe { std::mem::zeroed() };
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = interrupted as *const () as usize;
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            if unsafe { libc::sigaction(signal, &action, &mut old) } != 0 {
                return Err(io::Error::last_os_error());
            }
            installed.0.push((signal, old));
        }
        Ok(installed)
    }
    fn ignore(&self) {
        for (signal, _) in &self.0 {
            // Cleanup is not interruptible: all owned groups must be reaped.
            unsafe { libc::signal(*signal, libc::SIG_IGN) };
        }
    }
    fn pending(&self) -> Option<i32> {
        match PENDING_SIGNAL.load(Ordering::Relaxed) {
            0 => None,
            signal => Some(signal),
        }
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, old) in &self.0 {
            unsafe { libc::sigaction(*signal, old, std::ptr::null_mut()) };
        }
    }
}

pub(super) struct OwnedGroup {
    child: Child,
    reaped: bool,
}
impl OwnedGroup {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        // Only async-signal-safe setsid runs between fork and exec. The leader
        // is both session and process-group owner, matching start_new_session.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().map_err(|error| {
            io::Error::new(error.kind(), format!("launch {command:?}: {error}"))
        })?;
        #[cfg(test)]
        TRACE.with(|trace| {
            trace.borrow_mut().push(Trace::Spawn {
                pid: child.id() as i32,
                session: unsafe { libc::getsid(child.id() as i32) },
                program: command.get_program().to_owned(),
                args: command.get_args().map(OsString::from).collect(),
            })
        });
        Ok(Self {
            child,
            reaped: false,
        })
    }
    pub(super) fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    pub(super) fn exit_status(&self) -> io::Result<Option<i32>> {
        #[cfg(test)]
        TRACE.with(|trace| trace.borrow_mut().push(Trace::Wait(self.pid())));
        if self.reaped {
            return Err(io::Error::other("owned leader already reaped"));
        }
        // Never Child::try_wait here: the zombie leader reserves the PID/PGID
        // until the last group signal, preventing a reused-group signal.
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } == -1
            {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if unsafe { info.si_pid() } == 0 {
                return Ok(None);
            }
            let status = unsafe { info.si_status() };
            return Ok(Some(if info.si_code == libc::CLD_EXITED {
                status
            } else {
                -status
            }));
        }
    }

    fn signal(&self, signal: i32) -> io::Result<()> {
        #[cfg(test)]
        TRACE.with(|trace| {
            trace.borrow_mut().push(Trace::Signal {
                pid: self.pid(),
                signal,
            })
        });
        if unsafe { libc::kill(-self.pid(), signal) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn stop(&mut self) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        self.signal(libc::SIGTERM)?;
        let mut clock = GraceClock::start();
        while self.exit_status()?.is_none() {
            if clock.elapsed() >= GRACE {
                #[cfg(test)]
                TRACE.with(|trace| {
                    trace.borrow_mut().push(Trace::GraceExpired {
                        pid: self.pid(),
                        waited: clock.elapsed(),
                    })
                });
                break;
            }
            clock.poll();
        }
        self.signal(libc::SIGKILL)?;
        // From now on, no more group signals, even if a later status write fails.
        self.child.wait()?;
        self.reaped = true;
        loop {
            let result = unsafe { libc::waitpid(-self.pid(), std::ptr::null_mut(), 0) };
            if result >= 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                break;
            }
            return Err(error);
        }
        Ok(())
    }
}
impl Drop for OwnedGroup {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("batch: group {} cleanup: {error}", self.pid());
        }
    }
}

struct Running {
    group: OwnedGroup,
    output: PathBuf,
    started: Instant,
    status: Option<i32>,
}

fn exclusive(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}
fn record(output: &Path, status: i32) -> io::Result<()> {
    writeln!(exclusive(&sidecar(output, ".status"))?, "{status}")
}

pub(super) fn run_batch(options: Options) -> Result<i32> {
    // Invalid settings must not alter files, signal handlers or subreaper state.
    options.validate()?;
    enable_subreaper()?;
    let plans = options.plan();
    for plan in &plans {
        let output = &plan.output;
        for path in [
            output.clone(),
            sidecar(output, ".json"),
            sidecar(output, ".err"),
            sidecar(output, ".status"),
        ] {
            match fs::symlink_metadata(&path) {
                Ok(_) => return Err(format!("refusing to overwrite {}", path.display()).into()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let signals = Signals::install()?;
    let mut children: Vec<Running> = Vec::new();
    let execution = (|| -> Result<i32> {
        for plan in plans {
            let output = plan.output;
            if let Some(signal) = signals.pending() {
                return Ok(128 + signal);
            }
            fs::create_dir_all(output.parent().ok_or("output needs a parent directory")?)?;
            let stdout = exclusive(&sidecar(&output, ".json"))?;
            let stderr = exclusive(&sidecar(&output, ".err"))?;
            let group = OwnedGroup::spawn(
                Command::new(&options.vmm)
                    .args(plan.args)
                    .current_dir(&options.root)
                    .stdout(stdout)
                    .stderr(stderr),
            )?;
            children.push(Running {
                group,
                output,
                started: Instant::now(),
                status: None,
            });
        }
        while children.iter().any(|child| child.status.is_none()) {
            if let Some(signal) = signals.pending() {
                return Ok(128 + signal);
            }
            for child in &mut children {
                if child.status.is_some() {
                    continue;
                }
                let mut status = child.group.exit_status()?;
                if status.is_none() && child.started.elapsed().as_secs_f64() >= options.timeout {
                    status = Some(124);
                }
                if let Some(status) = status {
                    child.group.stop()?;
                    child.status = Some(status);
                    record(&child.output, status)?;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(0)
    })();
    let mut result = match execution {
        Ok(result) => result,
        Err(error) => {
            eprintln!("batch: {error}");
            1
        }
    };
    signals.ignore();
    // First clean EVERY group, then write any fallback statuses. Neither a
    // status-file error nor an earlier cleanup error can skip later children.
    for child in &mut children {
        if let Err(error) = child.group.stop() {
            eprintln!("batch: group {} cleanup: {error}", child.group.pid());
            if result == 0 {
                result = 1;
            }
        }
    }
    for child in &mut children {
        if child.status.is_none() {
            if let Err(error) = record(&child.output, if result == 0 { 1 } else { result }) {
                eprintln!("batch: status {}: {error}", child.output.display());
                if result == 0 {
                    result = 1;
                }
            }
        }
    }
    Ok(result)
}
