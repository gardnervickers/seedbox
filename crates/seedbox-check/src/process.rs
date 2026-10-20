use super::policy::Environment;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Job {
    pub name: String,
    pub args: Vec<String>,
    pub environment: Environment,
    pub cwd: PathBuf,
    pub capture: bool,
    pub timeout: Option<Duration>,
}
impl Job {
    pub fn new(name: &str, args: Vec<String>, environment: &Environment, cwd: &Path) -> Self {
        Self {
            name: name.into(),
            args,
            environment: environment.clone(),
            cwd: cwd.into(),
            capture: false,
            timeout: None,
        }
    }
}
#[derive(Default, Debug)]
pub struct Outcome {
    pub code: i32,
    pub stdout: String,
}
// Narrow subprocess boundary, permitting command-spy tests without host probes.
pub trait Commands {
    fn execute(&mut self, job: &Job) -> io::Result<Outcome>;
}
#[cfg(target_os = "linux")]
static SUBREAPER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);
extern "C" fn interrupted(signal: libc::c_int) {
    INTERRUPTED.store(signal, Ordering::Relaxed);
}
pub struct Real;
impl Real {
    pub fn install_signals() -> io::Result<Self> {
        // Only the standalone executable (or an isolated test subprocess)
        // installs process-global signal/subreaper state.
        #[cfg(target_os = "linux")]
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_os = "linux")]
        SUBREAPER.store(true, Ordering::Relaxed);
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = interrupted as *const () as usize;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
            }
            if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self)
    }
}
impl Commands for Real {
    fn execute(&mut self, job: &Job) -> io::Result<Outcome> {
        self.execute_until(job, |start| {
            job.timeout.is_some_and(|limit| start.elapsed() >= limit)
        })
    }
}
impl Real {
    // The deadline predicate permits event-driven timeout injection in boundary
    // tests without racing fixture readiness against a short wall-clock delay.
    pub fn execute_until(
        &mut self,
        job: &Job,
        expired: impl Fn(Instant) -> bool,
    ) -> io::Result<Outcome> {
        use std::os::unix::process::CommandExt;
        if INTERRUPTED.load(Ordering::Relaxed) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "coordinator interrupted",
            ));
        }
        let (program, args) = job
            .args
            .split_first()
            .ok_or_else(|| io::Error::other("empty command"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&job.cwd)
            .env_clear()
            .envs(&job.environment)
            .stdin(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdout(if job.capture {
                Stdio::piped()
            } else {
                Stdio::from(std::io::stderr())
            })
            .process_group(0);
        // Through the process-wide exec-file scope: the checker's own tests
        // write executables that sibling tests then run.
        // Sequential coordinator jobs own newly adopted children, never children
        // already present from another owner or an earlier failed cleanup.
        #[cfg(target_os = "linux")]
        let previous_children = if SUBREAPER.load(Ordering::Relaxed) {
            Some(direct_children())
        } else {
            None
        };
        let child = seedbox_exec::spawn(&mut command)
            .map_err(|e| io::Error::new(e.kind(), format!("{program}: {e}")))?;
        let mut owned = Owned {
            child,
            cleaned: false,
            #[cfg(target_os = "linux")]
            previous_children,
        };
        let captured = Arc::new(Mutex::new(Vec::new()));
        let reader = owned.child.stdout.take().map(|mut pipe| {
            let captured = captured.clone();
            std::thread::spawn(move || -> io::Result<()> {
                let mut chunk = [0; 8192];
                loop {
                    let count = match pipe.read(&mut chunk) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        result => result?,
                    };
                    if count == 0 {
                        return Ok(());
                    }
                    captured.lock().unwrap().extend_from_slice(&chunk[..count]);
                }
            })
        });
        let start = Instant::now();
        let result = loop {
            let signal = INTERRUPTED.load(Ordering::Relaxed);
            if signal != 0 {
                break Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("interrupted by signal {signal}"),
                ));
            }
            if expired(start) {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} timed out", job.name),
                ));
            }
            // Observe without reaping: reserve the leader PID until its group is
            // cleaned. Killing a group after try_wait() would permit PID reuse.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    owned.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break Err(e);
            }
            if info.si_signo != 0 {
                break Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let status = owned.cleanup();
        // A failed containment may leave a pipe writer alive. Snapshot bytes
        // already collected instead of joining that reader indefinitely.
        let output_result = if status.is_ok() {
            reader
                .map(|r| {
                    r.join()
                        .map_err(|_| io::Error::other("stdout reader panicked"))?
                })
                .transpose()
                .map(|_| ())
        } else {
            Ok(())
        };
        let output = {
            let mut bytes = captured.lock().unwrap();
            if status.is_ok() {
                std::mem::take(&mut *bytes)
            } else {
                bytes[bytes.len().saturating_sub(64 * 1024)..].to_vec()
            }
        };
        let status = finish(job, result, status, output_result, &output)?;
        let stdout = String::from_utf8(output).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: captured stdout is not valid UTF-8", job.name),
            )
        })?;
        Ok(Outcome {
            code: status.code().unwrap_or(1),
            stdout,
        })
    }
}
fn finish(
    job: &Job,
    result: io::Result<()>,
    status: io::Result<std::process::ExitStatus>,
    output_result: io::Result<()>,
    output: &[u8],
) -> io::Result<std::process::ExitStatus> {
    if result.is_ok() && status.is_ok() && output_result.is_ok() {
        return status;
    }
    let primary = result.err();
    let cleanup = status.as_ref().err();
    let kind = primary
        .as_ref()
        .map(io::Error::kind)
        .or_else(|| cleanup.map(io::Error::kind))
        .or_else(|| output_result.as_ref().err().map(io::Error::kind))
        .unwrap_or(io::ErrorKind::Other);
    let mut message = primary
        .map(|e| e.to_string())
        .unwrap_or_else(|| match &status {
            Ok(status) => format!("{}: {status}", job.name),
            Err(_) => job.name.clone(),
        });
    if let Some(error) = cleanup {
        message.push_str(&format!("\nprocess containment failed: {error}"));
    }
    if let Err(error) = output_result {
        message.push_str(&format!("\nstdout capture failed: {error}"));
    }
    let tail = &output[output.len().saturating_sub(64 * 1024)..];
    Err(io::Error::new(
        kind,
        format!(
            "{message}\ncaptured stdout (last {} bytes):\n{}",
            tail.len(),
            String::from_utf8_lossy(tail)
        ),
    ))
}

#[cfg(target_os = "linux")]
fn direct_children() -> io::Result<std::collections::HashSet<i32>> {
    let mut children = std::collections::HashSet::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let text = match std::fs::read_to_string(task?.path().join("children")) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A task can exit during enumeration. Its children move to a
                // surviving task. A missing file on a live task is unsupported.
                continue;
            }
            Err(error) => return Err(error),
        };
        for pid in text.split_whitespace() {
            children.insert(pid.parse().map_err(io::Error::other)?);
        }
    }
    // Unlike a disappearing sibling task, the calling task must be present.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    std::fs::read_to_string(format!("/proc/self/task/{tid}/children"))?;
    Ok(children)
}

fn signal(pid: i32, signal: i32) -> io::Result<()> {
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(io::Error::new(
            error.kind(),
            format!("signal {signal} to {pid}: {error}"),
        ))
    }
}

struct Owned {
    child: std::process::Child,
    cleaned: bool,
    #[cfg(target_os = "linux")]
    previous_children: Option<io::Result<std::collections::HashSet<i32>>>,
}
impl Owned {
    fn cleanup(&mut self) -> io::Result<std::process::ExitStatus> {
        if self.cleaned {
            return self.child.wait();
        }
        #[cfg(target_os = "linux")]
        if self.previous_children.is_some() {
            let status = self.cleanup_tree_with(direct_children, signal);
            self.cleaned = true;
            return status;
        }
        // The unreaped leader reserves its private process-group identity.
        let killed = signal(-(self.child.id() as i32), libc::SIGKILL);
        self.cleaned = true;
        killed?;
        self.child.wait()
    }

    #[cfg(target_os = "linux")]
    fn cleanup_tree_with(
        &mut self,
        mut children: impl FnMut() -> io::Result<std::collections::HashSet<i32>>,
        mut send: impl FnMut(i32, i32) -> io::Result<()>,
    ) -> io::Result<std::process::ExitStatus> {
        let leader = self.child.id() as i32;
        let (previous, mut failure) = match self.previous_children.take().unwrap() {
            Ok(previous) => (Some(previous), None),
            Err(error) => (None, Some(error)),
        };
        let start = Instant::now();
        let mut notified = std::collections::HashSet::new();
        let mut owned = std::collections::HashSet::new();
        let mut forced = false;
        let mut group_kill_failed = false;
        let mut descendant_kill_failed = false;
        if let Err(error) = send(-leader, libc::SIGTERM) {
            failure.get_or_insert(error);
        }
        loop {
            // Observe exit before enumerating: exiting parents have already
            // transferred their orphans when waitid reports their exit.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    leader as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let exited = info.si_signo != 0;
            let current = match previous.as_ref().map(|_| children()).transpose() {
                Ok(current) => current.unwrap_or_default(),
                Err(error) => {
                    failure.get_or_insert(error);
                    Default::default()
                }
            };
            if !forced && (failure.is_some() || start.elapsed() >= Duration::from_secs(2)) {
                // Always retain group SIGKILL, including without proc children.
                if let Err(error) = send(-leader, libc::SIGKILL) {
                    group_kill_failed = true;
                    failure.get_or_insert(error);
                }
                forced = true;
            }
            let mut pending = false;
            let mut reaped = false;
            owned.extend(
                current
                    .difference(previous.as_ref().unwrap_or(&Default::default()))
                    .copied()
                    .filter(|pid| *pid != leader),
            );
            for pid in owned.clone() {
                if forced || notified.insert(pid) {
                    if let Err(error) =
                        send(pid, if forced { libc::SIGKILL } else { libc::SIGTERM })
                    {
                        descendant_kill_failed |= forced;
                        failure.get_or_insert(error);
                    }
                }
                // Only wait on this job's direct adopted children. ECHILD for
                // the entire coordinator says nothing about this job's cleanup.
                let mut status = 0;
                let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if rc == pid {
                    notified.remove(&pid);
                    owned.remove(&pid);
                    reaped = true;
                } else if rc == 0 {
                    pending = true;
                } else {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        owned.remove(&pid);
                        notified.remove(&pid);
                    } else {
                        failure.get_or_insert(error);
                    }
                }
            }
            if exited && !pending && !reaped {
                let status = self.child.wait()?;
                return failure.map_or(Ok(status), Err);
            }
            if forced && (group_kill_failed || (exited && descendant_kill_failed)) {
                if let Some(error) = failure.take() {
                    // Permissions/visibility failures are containment errors,
                    // never a claim that an unkillable descendant was reaped.
                    if exited {
                        let _ = self.child.wait();
                    }
                    return Err(error);
                }
            }
            if start.elapsed() >= Duration::from_secs(10) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned process tree did not reap after SIGKILL",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        // Never signal a potentially reused PID after successful reaping.
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Cpu {
    pub user: f64,
    pub system: f64,
}
#[derive(Clone, Copy, Default, Debug)]
pub struct CpuSample {
    pub own: Cpu,
    pub children: Cpu,
}
impl CpuSample {
    pub fn read() -> io::Result<Self> {
        fn usage(who: i32) -> io::Result<Cpu> {
            let mut value: libc::rusage = unsafe { std::mem::zeroed() };
            if unsafe { libc::getrusage(who, &mut value) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Cpu {
                user: value.ru_utime.tv_sec as f64 + value.ru_utime.tv_usec as f64 / 1_000_000.0,
                system: value.ru_stime.tv_sec as f64 + value.ru_stime.tv_usec as f64 / 1_000_000.0,
            })
        }
        Ok(Self {
            own: usage(libc::RUSAGE_SELF)?,
            children: usage(libc::RUSAGE_CHILDREN)?,
        })
    }
}
pub fn timing_row(label: &str, elapsed: f64, before: CpuSample, after: CpuSample) -> String {
    let user = after.own.user + after.children.user - before.own.user - before.children.user;
    let system =
        after.own.system + after.children.system - before.own.system - before.children.system;
    format!("timing: portable/{label}: {elapsed:.3} seconds (user {user:.3}, sys {system:.3})")
}
pub fn timed<C: Commands, T>(
    checks: &mut Checks<C>,
    label: &str,
    action: impl FnOnce(&mut Checks<C>) -> Result<T, String>,
) -> Result<T, String> {
    let started = Instant::now();
    let before = CpuSample::read().map_err(|e| format!("timing {label}: {e}"))?;
    let result = action(checks);
    // Emit a row for failed commands and discovery errors as well as success.
    let after = CpuSample::read().map_err(|e| format!("timing {label}: {e}"))?;
    let row = timing_row(label, started.elapsed().as_secs_f64(), before, after);
    eprintln!("{row}");
    #[cfg(test)]
    checks.timings.push(row);
    result
}
pub struct Checks<C: Commands> {
    pub commands: C,
    #[cfg(test)]
    pub timings: Vec<String>,
    pub failed: bool,
    pub root: PathBuf,
    pub environment: Environment,
}
impl<C: Commands> Checks<C> {
    pub fn new(commands: C, root: PathBuf, mut environment: Environment) -> Self {
        // Direct coordinator invocation has the same bounded-output policy as
        // harness/check.sh, without changing the caller's interactive shell.
        environment.insert("CARGO_INCREMENTAL".into(), "0".into());
        Self {
            commands,
            #[cfg(test)]
            timings: Vec::new(),
            failed: false,
            root,
            environment,
        }
    }
    pub fn job(&self, name: &str, args: Vec<String>) -> Job {
        Job::new(name, args, &self.environment, &self.root)
    }
    pub fn run(&mut self, job: Job) -> Option<String> {
        let start = Instant::now();
        let display: Vec<_> = job
            .args
            .iter()
            .map(|v| {
                if v.starts_with("PATH=") {
                    "PATH=<development shell>"
                } else {
                    v
                }
            })
            .collect();
        eprintln!("running: {}: {:?}", job.name, display);
        let result = self.commands.execute(&job);
        let passed = result.as_ref().is_ok_and(|o| o.code == 0);
        match &result {
            Ok(out) if job.capture => {
                for line in out.stdout.lines() {
                    match serde_json::from_str::<serde_json::Value>(line) {
                        Ok(value) if value["reason"] == "compiler-message" => {
                            eprint!("{}", value["message"]["rendered"].as_str().unwrap_or(""))
                        }
                        Err(_) if !passed => eprintln!("{line}"),
                        _ => {}
                    }
                }
            }
            Err(e) => eprintln!("error: {}: {e}", job.name),
            _ => {}
        }
        self.failed |= !passed;
        println!(
            "{}: {} ({:.1}s)",
            job.name,
            if passed { "PASS" } else { "FAIL" },
            start.elapsed().as_secs_f64()
        );
        if passed {
            result.ok().map(|o| o.stdout)
        } else {
            None
        }
    }
    pub fn step(&mut self, name: &str, args: Vec<String>) -> bool {
        let job = self.job(name, args);
        self.run(job).is_some()
    }
}
pub fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).into()).collect()
}

/// Exclusive temporary directory; no runtime dependency on a fixture/backend.
pub struct Temporary(pub PathBuf);
impl Temporary {
    pub fn in_directory(parent: &Path, prefix: &str) -> io::Result<Self> {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let pattern = parent.join(format!("{prefix}XXXXXX"));
        let mut bytes =
            std::ffi::CString::new(pattern.as_os_str().as_bytes())?.into_bytes_with_nul();
        if unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        bytes.pop();
        Ok(Self(PathBuf::from(std::ffi::OsString::from_vec(bytes))))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            eprintln!("check: cleanup {}: {e}", self.0.display());
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod cleanup_tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::{
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    };

    #[test]
    fn job_scoped_cleanup_failures() {
        const MODE: &str = "SEEDBOX_CHECK_SCOPED_CLEANUP";
        const TEST: &str = "process::cleanup_tests::job_scoped_cleanup_failures";
        match std::env::var(MODE).as_deref() {
            Ok("leaf") => {
                unsafe {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                }
                let mut stream = UnixStream::connect("scope.sock").unwrap();
                stream.write_all(b"ready").unwrap();
                loop {
                    let mut request = [0];
                    stream.read_exact(&mut request).unwrap();
                    stream.write_all(&request).unwrap();
                }
            }
            Ok("owner") => {}
            _ => {
                let directory = tempfile::tempdir().unwrap();
                let mut environment: Environment = std::env::vars().collect();
                environment.insert(MODE.into(), "owner".into());
                let mut job = Job::new(
                    "scoped cleanup regression",
                    vec![
                        std::env::current_exe().unwrap().to_string_lossy().into(),
                        "--exact".into(),
                        TEST.into(),
                        "--nocapture".into(),
                    ],
                    &environment,
                    directory.path(),
                );
                job.capture = true;
                job.timeout = Some(Duration::from_secs(60));
                let output = Real.execute(&job).unwrap();
                assert_eq!(output.code, 0, "{}", output.stdout);
                return;
            }
        }
        let mut real = Real::install_signals().unwrap();
        let listener = UnixListener::bind("scope.sock").unwrap();
        listener.set_nonblocking(true).unwrap();
        let spawn = || {
            let previous = direct_children().unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST, "--nocapture"])
                .env(MODE, "leaf")
                .process_group(0)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            let child = seedbox_exec::spawn(&mut command).unwrap();
            let (mut stream, _) =
                seedbox_test_time::HangGuard::new("owned leaf ready", Duration::from_secs(30))
                    .wait_for(|| match listener.accept() {
                        Ok(pair) => Some(pair),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
                        Err(error) => panic!("accept: {error}"),
                    })
                    .unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut ready = [0; 5];
            stream.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
            (
                Owned {
                    child,
                    cleaned: false,
                    previous_children: Some(Ok(previous)),
                },
                stream,
            )
        };
        // A real live child from a failed containment must remain outside later
        // jobs. Inject EPERM at the signal boundary without privileged fixtures.
        let (mut leftover, mut stream) = spawn();
        let error = leftover
            .cleanup_tree_with(direct_children, |_, _| {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            })
            .unwrap_err();
        leftover.cleaned = true;
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        for _ in 0..2 {
            let job = Job::new(
                "later successful job",
                words(&["sh", "-c", "exit 0"]),
                &std::env::vars().collect(),
                &std::env::current_dir().unwrap(),
            );
            assert_eq!(real.execute(&job).unwrap().code, 0);
            // A positive reply establishes that cleanup neither killed nor
            // stole the wait status of this pre-existing child.
            stream.write_all(b"a").unwrap();
            let mut reply = [0];
            stream.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"a");
        }
        signal(-(leftover.child.id() as i32), libc::SIGKILL).unwrap();
        leftover.child.wait().unwrap();

        for missing_at_spawn in [false, true] {
            let (mut missing_proc, _stream) = spawn();
            if missing_at_spawn {
                missing_proc.previous_children = Some(Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "proc children unavailable at spawn",
                )));
            }
            let leader = missing_proc.child.id() as i32;
            let mut group_killed = false;
            let error = missing_proc
                .cleanup_tree_with(
                    || {
                        assert!(
                            !missing_at_spawn,
                            "cannot discover ownership without a baseline"
                        );
                        Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "proc children unavailable",
                        ))
                    },
                    |pid, sig| {
                        if pid == -leader && sig == libc::SIGKILL {
                            group_killed = true;
                        }
                        signal(pid, sig)
                    },
                )
                .unwrap_err();
            missing_proc.cleaned = true;
            assert!(group_killed);
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            // ECHILD for this exact PID proves group fallback killed AND reaped it.
            assert_eq!(
                unsafe { libc::waitpid(leader, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }

        let job = Job::new("original job", vec![], &Environment::new(), Path::new("."));
        for kind in [io::ErrorKind::TimedOut, io::ErrorKind::Interrupted] {
            let error = finish(
                &job,
                Err(io::Error::new(kind, "original failure")),
                Err(io::Error::from_raw_os_error(libc::EPERM)),
                Ok(()),
                b"original captured stdout tail",
            )
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            let message = error.to_string();
            assert!(message.contains("original failure"));
            assert!(message.contains("process containment failed:"));
            assert!(message.contains("Operation not permitted"));
            assert!(message.contains("original captured stdout tail"));
        }
    }
}
