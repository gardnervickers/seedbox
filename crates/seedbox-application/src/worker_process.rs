//! Shared ownership of a retained engine process and its execution output.
//! Platform adapters hold host guards outside this lifetime and supply only
//! backend arguments, admission and report validation.

use crate::{
    execution::{CancellationToken, ExecutionReport, ExecutionRequest, StopReason},
    host_control::Endpoint,
    Error, Result,
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::{Child, Command},
    time::Duration,
};

pub(crate) fn incomplete(reason: StopReason) -> ExecutionReport {
    ExecutionReport {
        host_wait_ns: 0,
        simulated_time_ns: None,
        reason,
        workload_exit: None,
        witness: None,
        full_state: None,
        faults: Vec::new(),
        detail: None,
    }
}

pub(crate) struct WorkerLaunch {
    pub(crate) command: Command,
    engine: PathBuf,
    report: PathBuf,
    endpoint: Endpoint,
    spawn_fix: &'static str,
    native_scratch: Option<PathBuf>,
}

/// Launch request beside `world.json` in the run output; the x86 worker reads it.
pub(crate) const WORKER_REQUEST_FILE: &str = "worker-request.json";

impl WorkerLaunch {
    pub(crate) fn prepare_driver_with_runtime(
        request: &ExecutionRequest<'_>,
        runtime: Option<&crate::driver_world::binding::Binding>,
    ) -> Result<Self> {
        Self::prepare_driver_impl(request, runtime)
    }
    fn prepare_driver_impl(
        request: &ExecutionRequest<'_>,
        runtime: Option<&crate::driver_world::binding::Binding>,
    ) -> Result<Self> {
        let engine = request
            .artifacts
            .get(&request.prepared.profile.engine)
            .ok_or_else(|| Error::Integrity("missing retained Driver executable".into()))?;
        if fs::metadata(engine)?.permissions().mode() & 0o7777 != 0o500 {
            fs::set_permissions(engine, fs::Permissions::from_mode(0o500))?;
        }
        let endpoint = Endpoint::new(request.output)?;
        let worker = crate::driver_world::retained::request(request, &endpoint.socket(), runtime)?;
        let path = request.output.join(WORKER_REQUEST_FILE);
        fs::write(&path, serde_json::to_vec_pretty(&worker)?)?;
        let report = request.output.join("vmm.json");
        let mut command = Command::new(engine);
        command.args(["__engine", crate::engine::DRIVER_WORLD_V1]);
        command
            .arg("--owner-pipe")
            .arg("--request")
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .env_clear()
            .stdout(fs::File::create(&report)?)
            .stderr(fs::File::create(request.output.join("vmm.stderr"))?)
            .process_group(0);
        Ok(Self {
            command,
            engine: engine.clone(),
            report,
            endpoint,
            spawn_fix: "use the retained Driver worker CLI for this host",
            native_scratch: Some(worker.output.join(crate::driver_world::NATIVE_SCRATCH)),
        })
    }

    /// Launch through an explicit CLI monitor entry. The core adapter supplies
    /// the monitor executable; this library never discovers launch environment.
    /// Call `wait` in a blocking execution context. Dropping the result cancels
    /// asynchronously and transfers monitor reaping to the owned reaper thread.
    pub(crate) fn spawn_with_monitor(
        self,
        monitor: &std::path::Path,
        session_lease: Option<&std::path::Path>,
    ) -> Result<RunningWorker<crate::process_lifetime::OwnedExecution>> {
        let started = seedbox_engine_protocol::host_clock::Instant::now()?;
        let scratch = self
            .native_scratch
            .as_ref()
            .map(|path| {
                let parent = path.parent().expect("native scratch parent");
                fs::create_dir_all(parent)?;
                // A fixed name inside a fresh owned execution, never adoption of an
                // old directory: create_new semantics reject collisions/symlinks.
                tempfile::Builder::new()
                    .prefix(crate::driver_world::NATIVE_SCRATCH)
                    .rand_bytes(0)
                    .tempdir_in(parent)
            })
            .transpose()?;
        let mut arguments: Vec<_> = self
            .command
            .get_args()
            .filter(|arg| *arg != "--owner-pipe")
            .map(std::ffi::OsStr::to_owned)
            .collect();
        // Every Driver launch retains the inherited lease through its monitor.
        arguments.insert(0, seedbox_machine::process_lease::ENTRY.into());
        let environment: Vec<_> = self
            .command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        // Drop Command's original stdio copies before starting the monitor.
        drop(self.command);
        let endpoint = std::sync::Arc::new(self.endpoint);
        let mut process = crate::process_lifetime::OwnedExecution::spawn(
            monitor,
            &self.engine,
            &arguments,
            &environment,
            fs::File::create(&self.report)?,
            fs::File::create(self.report.with_file_name("vmm.stderr"))?,
            crate::process_lifetime::LeasePaths {
                execution: &self
                    .report
                    .with_file_name(crate::process_lifetime::EXECUTION_LEASE_FILE),
                session: session_lease,
            },
        )
        .map_err(|error| {
            Error::Setup(format!(
                "engine monitor {}: {error}; {}",
                monitor.display(),
                self.spawn_fix
            ))
        })?;
        // Endpoint owns a scratch clock/socket directory as well as a session
        // locator. Its destructor must follow the same monitor exit barrier.
        process.retain_until_join(endpoint.clone());
        if let Some(scratch) = scratch {
            process.retain_until_join(scratch);
        }
        Ok(RunningWorker {
            process,
            report: self.report,
            started,
            _endpoint: endpoint,
        })
    }

    /// Historical standalone-launch ownership, retained until all callers use
    /// the explicit monitor boundary. The Driver adapter uses spawn_with_monitor.
    /// The caller must acquire any host lease before spawning. The returned
    /// worker stops and reaps its group before the caller releases its host guard.
    #[cfg(test)]
    pub(crate) fn spawn_worker(mut self) -> Result<RunningWorker> {
        if self.native_scratch.is_some() {
            return Err(Error::Setup(
                "retained Driver workers require monitored launch for native scratch ownership"
                    .into(),
            ));
        }
        let started = seedbox_engine_protocol::host_clock::Instant::now()?;
        let child = crate::exec_files::spawn(&mut self.command).map_err(|error| {
            Error::Setup(format!(
                "engine worker {}: {error}; {}",
                self.engine.display(),
                self.spawn_fix
            ))
        })?;
        Ok(RunningWorker {
            process: OwnedProcess(Some(child)),
            report: self.report,
            started,
            _endpoint: std::sync::Arc::new(self.endpoint),
        })
    }
}

/// Include a bounded diagnostic tail when a worker exits without a usable report.
/// The original stderr file remains available for the complete diagnostic.
fn worker_failure(report: &std::path::Path) -> String {
    const LIMIT: u64 = 8192;
    let failure = crate::execution::VMM_FAILURE_MESSAGE;
    let tail = (|| -> std::io::Result<String> {
        let mut file = fs::File::open(report.with_file_name("vmm.stderr"))?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(LIMIT)))?;
        let mut bytes = Vec::new();
        file.take(LIMIT).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    })();
    match tail {
        Ok(tail) if !tail.is_empty() => format!("{failure}; worker stderr: {tail}"),
        _ => failure.into(),
    }
}

pub(crate) struct RunningWorker<P = OwnedProcess> {
    // Monitored cleanup retains its execution lease independently of endpoint
    // removal, including when cancellation drops this handle on an async task.
    process: P,
    report: PathBuf,
    started: seedbox_engine_protocol::host_clock::Instant,
    _endpoint: std::sync::Arc<Endpoint>,
}

impl RunningWorker<crate::process_lifetime::OwnedExecution> {
    #[cfg(feature = "test-support")]
    pub(crate) fn with_failure_injector(
        self,
        injector: &crate::process_lifetime::WorkerFailureInjector,
    ) -> Result<Self> {
        self.process
            .bind_failure_injector(injector)
            .map_err(|error| {
                Error::Execution(format!("bind original worker failure injector: {error}"))
            })?;
        Ok(self)
    }

    /// Carry runtime/scratch deletion guards into the detached monitor join if
    /// the caller drops this execution on an async cancellation path.
    pub(crate) fn retain_until_join(mut self, guard: impl Send + 'static) -> Self {
        self.process.retain_until_join(guard);
        self
    }
}
impl<P: ProcessLifetime> RunningWorker<P> {
    pub(crate) fn wait(
        self,
        cancellation: &CancellationToken,
        wall_time_ns: u64,
        parse_report: impl FnOnce(&[u8]) -> Result<ExecutionReport>,
    ) -> Result<ExecutionReport> {
        self.wait_with_sampling(
            cancellation,
            wall_time_ns,
            parse_report,
            |worker| Ok(worker.started.elapsed()?),
            |worker| worker._endpoint.waited(),
        )
    }

    // Private sampling keeps the real clock reads and their failure/cleanup
    // order in production, while tests can advance elapsed and accounted wait
    // independently without changing the public worker or clock contracts.
    fn wait_with_sampling(
        mut self,
        cancellation: &CancellationToken,
        wall_time_ns: u64,
        parse_report: impl FnOnce(&[u8]) -> Result<ExecutionReport>,
        mut elapsed: impl FnMut(&Self) -> Result<Duration>,
        mut waited: impl FnMut(&Self) -> Result<Duration>,
    ) -> Result<ExecutionReport> {
        let completion = (|| loop {
            if let Some(success) = self.process.exited()? {
                let report = fs::read(&self.report)
                    .map_err(Error::from)
                    .and_then(|bytes| parse_report(&bytes));
                return match (success, report) {
                    (true, report) => report,
                    (false, Ok(mut report)) => {
                        report.reason = StopReason::Error;
                        let failure = worker_failure(&self.report);
                        report.detail = Some(match report.detail {
                            Some(detail) => format!("{failure}; {detail}"),
                            None => failure,
                        });
                        Ok(report)
                    }
                    (false, Err(error)) => Err(Error::Execution(format!(
                        "{}; {error}",
                        worker_failure(&self.report)
                    ))),
                };
            }
            if cancellation.is_cancelled() {
                return Ok(incomplete(StopReason::Cancelled));
            }
            if elapsed(&self)?.saturating_sub(waited(&self)?) >= Duration::from_nanos(wall_time_ns)
            {
                return Ok(incomplete(StopReason::WallTimeLimit));
            }
            std::thread::sleep(Duration::from_millis(10));
        })();
        // Cleanup remains an execution error, but an already observed oracle
        // exit must survive it. Drop still retries cleanup before releasing ownership.
        let completion = match waited(&self) {
            Ok(waited) => completion.map(|mut report| {
                report.host_wait_ns = waited.as_nanos().min(u128::from(u64::MAX)) as u64;
                report
            }),
            Err(error) => finish_cleanup(completion, Err(error)),
        };
        finish_cleanup(completion, self.process.stop())
    }
}

fn finish_cleanup(
    completion: Result<ExecutionReport>,
    cleanup: Result<()>,
) -> Result<ExecutionReport> {
    match (completion, cleanup) {
        (completion, Ok(())) => completion,
        (Ok(mut report), Err(error)) => {
            report.reason = StopReason::Error;
            report.detail = Some(match report.detail {
                Some(previous) => format!("{previous}; engine cleanup: {error}"),
                None => format!("engine cleanup: {error}"),
            });
            Ok(report)
        }
        (Err(previous), Err(error)) => Err(Error::Execution(format!(
            "{previous}; engine cleanup: {error}"
        ))),
    }
}

pub(crate) trait ProcessLifetime {
    fn exited(&mut self) -> Result<Option<bool>>;
    fn stop(&mut self) -> Result<()>;
}
impl ProcessLifetime for crate::process_lifetime::OwnedExecution {
    fn exited(&mut self) -> Result<Option<bool>> {
        self.poll()?
            .map(|completion| {
                // Read any terminal oracle report before propagating cleanup
                // failure from stop(), preserving an already observed verdict.
                match &completion.worker {
                    Some(exit) => Ok(*exit == crate::process_lifetime::WorkerExit::Code(0)),
                    None => Err(Error::Execution(format!(
                        "worker exit not observed: {completion:?}; execution files must be kept"
                    ))),
                }
            })
            .transpose()
    }
    fn stop(&mut self) -> Result<()> {
        self.cancel();
        let completion = self.finish()?;
        if !completion.cleanup_authorized() {
            return Err(Error::Execution(format!(
                "engine cleanup failed: {completion:?}; execution files must be kept"
            )));
        }
        Ok(())
    }
}
impl ProcessLifetime for OwnedProcess {
    fn exited(&mut self) -> Result<Option<bool>> {
        OwnedProcess::exited(self)
    }
    fn stop(&mut self) -> Result<()> {
        OwnedProcess::stop(self)
    }
}

/// Keep the unreaped leader through the final group signal. Reaping it early
/// could let a different process reuse the group ID before cleanup.
pub(crate) struct OwnedProcess(Option<Child>);
impl OwnedProcess {
    fn exited(&self) -> Result<Option<bool>> {
        let child = self
            .0
            .as_ref()
            .ok_or_else(|| Error::Execution("engine worker already reaped".into()))?;
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
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        #[cfg(target_os = "linux")]
        return Ok(unsafe {
            (info.si_pid() != 0).then(|| info.si_code == libc::CLD_EXITED && info.si_status() == 0)
        });
        #[cfg(target_os = "macos")]
        return Ok(
            (info.si_pid != 0).then_some(info.si_code == libc::CLD_EXITED && info.si_status == 0)
        );
    }

    fn stop(&mut self) -> Result<()> {
        if self.0.is_none() {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        let exited = self.exited()?.is_some();
        if let Some(child) = self.0.as_mut() {
            if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } != 0 {
                let error = std::io::Error::last_os_error();
                #[cfg(target_os = "linux")]
                let finished_group = false;
                // Darwin reports a zombie's uid as 0, and killpg returns EPERM
                // when no live member remains. Our workers keep the caller uid.
                #[cfg(target_os = "macos")]
                let finished_group = exited && error.raw_os_error() == Some(libc::EPERM);
                if error.raw_os_error() != Some(libc::ESRCH) && !finished_group {
                    return Err(error.into());
                }
            }
            child.wait()?;
            self.0 = None;
        }
        Ok(())
    }
}
/// How long `Drop` retries a failing stop before reporting it.
const DROP_STOP_ATTEMPTS: u32 = 100;

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        for attempt in 1..=DROP_STOP_ATTEMPTS {
            match self.stop() {
                Ok(()) => return,
                Err(error) if attempt == DROP_STOP_ATTEMPTS => {
                    eprintln!("seedbox: engine cleanup: {error}");
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_failure_includes_bounded_stderr_without_requiring_it() {
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("vmm.json");
        assert_eq!(
            worker_failure(&report),
            crate::execution::VMM_FAILURE_MESSAGE
        );
        let stderr = directory.path().join("vmm.stderr");
        fs::write(
            &stderr,
            format!("{}\nerror: reboot transport closed\n", "x".repeat(20000)),
        )
        .unwrap();
        let failure = worker_failure(&report);
        assert!(failure.ends_with("error: reboot transport closed"));
        assert!(failure.len() < 8400);
        fs::write(&stderr, " \n").unwrap();
        assert_eq!(
            worker_failure(&report),
            crate::execution::VMM_FAILURE_MESSAGE
        );
    }

    #[test]
    fn cleanup_failure_preserves_the_observed_oracle_exit() {
        for code in [0, 7] {
            let mut report = incomplete(StopReason::Completed);
            report.workload_exit = Some(crate::execution::WorkloadExit {
                service: "oracle".into(),
                code,
            });
            let result =
                finish_cleanup(Ok(report), Err(Error::Execution("still running".into()))).unwrap();
            assert_eq!(result.workload_exit.unwrap().code, code);
            assert_eq!(result.reason, StopReason::Error);
            assert!(result.detail.unwrap().contains("engine cleanup"));
        }
    }

    fn worker(output: &std::path::Path, script: &str) -> RunningWorker {
        let report = output.join("vmm.json");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(std::process::Stdio::piped())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("test tool PATH"))
            .process_group(0)
            .stdout(fs::File::create(&report).unwrap())
            .stderr(fs::File::create(output.join("vmm.stderr")).unwrap());
        WorkerLaunch {
            command,
            engine: "/bin/sh".into(),
            report,
            endpoint: Endpoint::new(output).unwrap(),
            spawn_fix: "test worker",
            native_scratch: None,
        }
        .spawn_worker()
        .unwrap()
    }

    fn assert_reaped(pid: u32, output: &std::path::Path) {
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(!output.join("host-control").exists());
    }

    #[test]
    fn worker_failure_preserves_stderr_when_report_is_empty_or_invalid() {
        for script in [
            "echo 'error: reboot transport closed' >&2; exit 1",
            "echo 'error: reboot transport closed' >&2; printf '{invalid'; exit 1",
        ] {
            let output = tempfile::tempdir().unwrap();
            let child = worker(output.path(), script);
            let pid = child.process.0.as_ref().unwrap().id();
            let error = child
                .wait(&CancellationToken::default(), 5_000_000_000, |bytes| {
                    serde_json::from_slice::<serde_json::Value>(bytes)?;
                    Ok(incomplete(StopReason::Completed))
                })
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("worker stderr: error: reboot transport closed"),
                "{error}"
            );
            assert_reaped(pid, output.path());
        }
    }

    #[test]
    fn an_unsuccessful_worker_can_still_supply_a_valid_oracle_report() {
        let output = tempfile::tempdir().unwrap();
        let child = worker(output.path(), "printf result; exit 1");
        let pid = child.process.0.as_ref().unwrap().id();
        let report = child
            .wait(&CancellationToken::default(), 5_000_000_000, |bytes| {
                assert_eq!(bytes, b"result");
                let mut report = incomplete(StopReason::Completed);
                report.workload_exit = Some(crate::execution::WorkloadExit {
                    service: "oracle".into(),
                    code: 0,
                });
                Ok(report)
            })
            .unwrap();
        assert_eq!(report.workload_exit.unwrap().code, 0);
        assert_eq!(report.reason, StopReason::Error);
        assert!(report
            .detail
            .unwrap()
            .contains(crate::execution::VMM_FAILURE_MESSAGE));
        assert_reaped(pid, output.path());
    }

    #[test]
    fn observing_exit_preserves_ownership_until_cleanup() {
        let output = tempfile::tempdir().unwrap();
        let mut child = worker(output.path(), "printf result");
        let pid = child.process.0.as_ref().unwrap().id();
        seedbox_test_time::HangGuard::new(
            "owned worker reports its actual exit before explicit cleanup",
            Duration::from_secs(5),
        )
        .wait_for(|| child.process.exited().unwrap())
        .unwrap();
        assert_eq!(child.process.exited().unwrap(), Some(true));
        assert_eq!(child.process.exited().unwrap(), Some(true));
        child.process.stop().unwrap();
        child.process.stop().unwrap();
        drop(child);
        assert_reaped(pid, output.path());
    }

    #[test]
    fn cancelled_and_timed_out_workers_are_reaped_before_return() {
        for cancelled in [false, true] {
            let output = tempfile::tempdir().unwrap();
            let child = worker(output.path(), "exec cat");
            let pid = child.process.0.as_ref().unwrap().id();
            let token = CancellationToken::default();
            if cancelled {
                token.cancel();
            }
            let report = child
                .wait(&token, 1, |_| {
                    panic!("unfinished worker report must not be read")
                })
                .unwrap();
            assert_eq!(
                report.reason,
                if cancelled {
                    StopReason::Cancelled
                } else {
                    StopReason::WallTimeLimit
                }
            );
            assert_reaped(pid, output.path());
        }
    }

    #[test]
    fn intentional_waits_suspend_the_budget_but_not_cancellation() {
        use seedbox_test_time::{handshake, ControlledClock, HangGuard};
        for cancel in [false, true] {
            let output = tempfile::tempdir().unwrap();
            let child = worker(output.path(), "exec cat");
            let pid = child.process.0.as_ref().unwrap().id();
            // Keep a real owned child and its cleanup assertions, but make the
            // budget proof independent of host passage and scheduling.
            let elapsed = ControlledClock::new(Duration::from_millis(200));
            let accounted = ControlledClock::new(Duration::from_millis(200));
            let (controller, entered) = handshake();
            let guard = HangGuard::new(
                "owned worker completes a paused-budget iteration and its final cleanup",
                Duration::from_secs(5),
            );
            let token = CancellationToken::default();
            std::thread::scope(|scope| {
                let token_ref = &token;
                let sampled_elapsed = elapsed.clone();
                let sample_guard = guard.clone();
                let waiting = scope.spawn(move || {
                    let mut samples = 0;
                    let mut entered = Some(entered);
                    child.wait_with_sampling(
                        token_ref,
                        100_000_000,
                        |_| panic!("unfinished worker report must not be read"),
                        |_| {
                            let _ = sample_guard.remaining().expect("worker budget loop hung");
                            samples += 1;
                            if samples == 2 {
                                // Reaching the next sample proves the first
                                // 200ms elapsed/200ms wait iteration continued.
                                entered
                                    .take()
                                    .unwrap()
                                    .enter_and_wait(&sample_guard)
                                    .unwrap();
                            }
                            Ok(sampled_elapsed.now())
                        },
                        |_| Ok(accounted.now()),
                    )
                });
                controller.wait_entered(&guard).unwrap();
                if cancel {
                    token.cancel();
                } else {
                    // Close the logical wait: one exact remaining budget now
                    // elapses, while its accounted duration stays unchanged.
                    elapsed.advance_by(Duration::from_millis(100)).unwrap();
                }
                controller.proceed().unwrap();
                let report = waiting.join().unwrap().unwrap();
                assert_eq!(
                    report.reason,
                    if cancel {
                        StopReason::Cancelled
                    } else {
                        StopReason::WallTimeLimit
                    }
                );
                assert_eq!(report.host_wait_ns, 200_000_000);
            });
            assert_reaped(pid, output.path());
        }
    }

    #[test]
    fn report_errors_and_early_drop_clean_up_the_worker_and_endpoint() {
        for early_drop in [false, true] {
            let output = tempfile::tempdir().unwrap();
            let child = worker(
                output.path(),
                if early_drop {
                    "exec cat"
                } else {
                    "printf invalid"
                },
            );
            let pid = child.process.0.as_ref().unwrap().id();
            if early_drop {
                drop(child);
            } else {
                let error = child
                    .wait(&CancellationToken::default(), 5_000_000_000, |_| {
                        Err(Error::Integrity("invalid test report".into()))
                    })
                    .unwrap_err();
                assert!(error.to_string().contains("invalid test report"));
            }
            assert_reaped(pid, output.path());
        }
    }
}
