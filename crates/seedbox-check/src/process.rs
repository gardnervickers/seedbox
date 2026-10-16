use super::policy::Environment;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicI32, Ordering},
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
        let child = command
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("{program}: {e}")))?;
        let mut owned = Owned {
            child,
            cleaned: false,
        };
        let reader = owned.child.stdout.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                pipe.read_to_end(&mut bytes).map(|_| bytes)
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
            if job.timeout.is_some_and(|limit| start.elapsed() >= limit) {
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
        // On Linux the executable is a subreaper: collect adopted group
        // descendants too. The leader PID was reserved until kill_group().
        #[cfg(target_os = "linux")]
        loop {
            let rc = unsafe { libc::waitpid(-(owned.child.id() as i32), std::ptr::null_mut(), 0) };
            if rc > 0 {
                continue;
            }
            if rc < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        let output = reader
            .map(|r| {
                r.join()
                    .map_err(|_| io::Error::other("stdout reader panicked"))?
            })
            .transpose()?;
        result?;
        let status = status?;
        let stdout = match output {
            Some(bytes) => String::from_utf8(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: captured stdout is not valid UTF-8", job.name),
                )
            })?,
            None => String::new(),
        };
        Ok(Outcome {
            code: status.code().unwrap_or(1),
            stdout,
        })
    }
}
struct Owned {
    child: std::process::Child,
    cleaned: bool,
}
impl Owned {
    fn cleanup(&mut self) -> io::Result<std::process::ExitStatus> {
        // No try_wait(): even a finished leader reserves its identity until we
        // have signalled its private group. Drop uses the same order on unwind.
        if !self.cleaned {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            self.cleaned = true;
        }
        self.child.wait()
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
