//! Shared ownership of a retained engine process and its execution output.
//! Platform adapters hold host guards outside this lifetime and supply only
//! backend arguments, admission and report validation.

use crate::{
    engine::ExecutableKind,
    execution::{
        CancellationToken, ExecutionReport, ExecutionRequest, StopReason, Timebase, WitnessMode,
    },
    host_control::Endpoint,
    Error, Result,
};
use std::{
    fs,
    io::Read,
    os::fd::AsRawFd,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

/// Probe only explicit replacements. Normal retained replay stays lookup-only.
pub(crate) fn replacement_version(
    engine: &std::path::Path,
    supplied_path: &std::path::Path,
) -> Result<Option<String>> {
    replacement_version_with_timeout(engine, supplied_path, Duration::from_secs(2))
}

fn replacement_version_with_timeout(
    engine: &std::path::Path,
    supplied_path: &std::path::Path,
    timeout: Duration,
) -> Result<Option<String>> {
    use std::process::Stdio;
    let failure = |detail: &str| {
        Error::Setup(format!(
            "--vmm {}: {detail}; use a Seedbox CLI or standalone VMM executable",
            supplied_path.display()
        ))
    };
    let child = Command::new(engine)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| failure(&e.to_string()))?;
    let mut process = OwnedProcess(Some(child));
    let mut stdout = process
        .0
        .as_mut()
        .and_then(|child| child.stdout.take())
        .ok_or_else(|| failure("missing version output"))?;
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(failure(&std::io::Error::last_os_error().to_string()));
    }
    let start = Instant::now();
    let mut bytes = Vec::new();
    let mut finished: Option<bool> = None;
    loop {
        let mut buffer = [0; 1024];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if bytes.len() + n > 4096 {
                        return Err(failure("version output exceeds 4096 bytes"));
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(failure(&e.to_string())),
            }
        }
        if let Some(success) = finished {
            process.stop()?;
            if !success {
                return Ok(None);
            }
            let version = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.trim().strip_prefix("seedbox "))
                .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace));
            return Ok(version.map(str::to_owned));
        }
        // Drain once more after exit to include the last bytes in the pipe.
        finished = process.exited()?;
        if finished.is_none() {
            if start.elapsed() >= timeout {
                return Err(failure("version probe timed out"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub(crate) fn incomplete(reason: StopReason, timebase: Timebase) -> ExecutionReport {
    ExecutionReport {
        timebase: Some(timebase),
        reason,
        workload_exit: None,
        witness: None,
        full_state: None,
        faults: Vec::new(),
        detail: None,
        engine_qualification: None,
    }
}

pub(crate) struct WorkerLaunch {
    pub(crate) command: Command,
    engine: PathBuf,
    report: PathBuf,
    endpoint: Endpoint,
    spawn_fix: &'static str,
    timebase: Timebase,
}

impl WorkerLaunch {
    pub(crate) fn prepare(request: &ExecutionRequest<'_>) -> Result<Self> {
        let engine = request
            .artifacts
            .get(&request.prepared.profile.engine)
            .ok_or_else(|| Error::Integrity("missing retained engine executable".into()))?;
        fs::set_permissions(engine, fs::Permissions::from_mode(0o500))?;
        let world = request.output.join("world.json");
        fs::write(
            &world,
            serde_json::to_vec_pretty(&crate::vmm::world(request)?)?,
        )?;
        let report = request.output.join("vmm.json");
        let endpoint = Endpoint::new(request.output)?;
        let kind = crate::engine::recorded_launch(&request.prepared.profile)?;
        let mut command = crate::vmm::engine_command(engine, &request.prepared.profile)?;
        let timebase = if kind == ExecutableKind::ArmWorld {
            Timebase::Pv
        } else {
            crate::vmm::recorded_timebase(&request.prepared.profile)?
        };
        let spawn_fix = match kind {
            ExecutableKind::Standalone => "check the retained executable and host setup",
            ExecutableKind::InternalWorker => "use the retained CLI executable for this host",
            ExecutableKind::ArmWorld => "use the retained signed CLI",
        };
        let streams = request
            .prepared
            .guests
            .len()
            .checked_mul(2)
            .filter(|count| *count != 0)
            .ok_or_else(|| Error::Integrity("execution requires prepared machines".into()))?;
        command
            .arg("--config")
            .arg(&world)
            .arg("--seed")
            .arg(request.seed.to_string())
            .arg("--out")
            .arg(request.output.join(crate::vmm::MACHINE_OUTPUT_DIR))
            .arg("--host-control")
            .arg(endpoint.socket())
            .arg("--console-limit")
            .arg((1024 * 1024 * 1024 / streams).to_string())
            .env_clear()
            .stdout(fs::File::create(&report)?)
            .stderr(fs::File::create(request.output.join("vmm.stderr"))?)
            .process_group(0);
        if kind != ExecutableKind::ArmWorld {
            command
                .arg("--stream-console")
                // Bound observation metadata to at most 40 MiB across all streams.
                .arg("--log-chunk-limit")
                .arg((1_048_576 / streams).min(65_536).to_string());
        }
        if request.prepared.profile.witness_mode == WitnessMode::FullState {
            command.arg("--digest");
        }
        Ok(Self {
            command,
            engine: engine.clone(),
            report,
            endpoint,
            spawn_fix,
            timebase,
        })
    }

    /// The caller must acquire any host lease before spawning. The returned
    /// worker stops and reaps its group before the caller releases its host guard.
    pub(crate) fn spawn(mut self) -> Result<RunningWorker> {
        let started = Instant::now();
        let child = self.command.spawn().map_err(|error| {
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
            timebase: self.timebase,
            _endpoint: self.endpoint,
        })
    }
}

pub(crate) struct RunningWorker {
    // Field drop order keeps process cleanup ahead of endpoint removal.
    process: OwnedProcess,
    report: PathBuf,
    started: Instant,
    timebase: Timebase,
    _endpoint: Endpoint,
}

impl RunningWorker {
    pub(crate) fn wait(
        mut self,
        cancellation: &CancellationToken,
        wall_time_ns: u64,
        parse_report: impl FnOnce(&[u8]) -> Result<ExecutionReport>,
    ) -> Result<ExecutionReport> {
        let completion = (|| loop {
            if cancellation.is_cancelled() {
                return Ok(incomplete(StopReason::Cancelled, self.timebase));
            }
            if let Some(success) = self.process.exited()? {
                return if success {
                    parse_report(&fs::read(&self.report)?)
                } else {
                    Err(Error::Execution(crate::vmm::VMM_FAILURE_MESSAGE.into()))
                };
            }
            if self.started.elapsed() >= Duration::from_nanos(wall_time_ns) {
                return Ok(incomplete(StopReason::WallTimeLimit, self.timebase));
            }
            std::thread::sleep(Duration::from_millis(10));
        })();
        // Cleanup failure takes precedence over any apparent result. Drop keeps
        // retrying until the host adapter can safely release its guard.
        self.process.stop()?;
        completion
    }
}

/// Keep the unreaped leader through the final group signal. Reaping it early
/// could let a different process reuse the group ID before cleanup.
struct OwnedProcess(Option<Child>);
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
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        while self.0.is_some() {
            if let Err(error) = self.stop() {
                eprintln!("seedbox: waiting for engine cleanup: {error}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_probe_bounds_output_and_cleans_up_timeout() {
        let _lock = crate::preparation_command::PROCESS_TEST_LOCK
            .lock()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let engine = directory.path().join("engine");
        let probe = |body: &str, timeout| {
            fs::write(&engine, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&engine, fs::Permissions::from_mode(0o700)).unwrap();
            replacement_version_with_timeout(&engine, &engine, timeout)
        };
        assert_eq!(
            probe(
                "printf 'seedbox 0.2.0-dev+replacement\\n'",
                Duration::from_secs(2)
            )
            .unwrap()
            .as_deref(),
            Some("0.2.0-dev+replacement")
        );
        assert_eq!(probe("exit 2", Duration::from_secs(2)).unwrap(), None);
        assert_eq!(
            probe("printf 'another program\\n'", Duration::from_secs(2)).unwrap(),
            None
        );
        assert!(probe(
            "i=0; while [ $i -lt 5000 ]; do printf x; i=$((i+1)); done",
            Duration::from_secs(2)
        )
        .unwrap_err()
        .to_string()
        .contains("4096"));
        let pid_file = directory.path().join("pid");
        let body = format!("echo $$ > '{}'; while :; do :; done", pid_file.display());
        assert!(probe(&body, Duration::from_millis(100))
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        let pid: i32 = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn prepared_launch_uses_recorded_dispatch_clock_and_retention_limits() {
        use crate::{
            config::{Check, Limits, Network, Schedule},
            execution::{ExecutionProfile, PreparedInputs},
            store::ContentDigest,
        };
        use std::collections::BTreeMap;
        let internal = BTreeMap::from([
            (
                "engine_executable_kind".into(),
                crate::engine::INTERNAL_WORKER_V1.into(),
            ),
            ("simulator_version".into(), "0.1.0-dev.1".into()),
            ("host_os".into(), "linux".into()),
            ("host_arch".into(), "x86_64".into()),
            ("guest_arch".into(), "x86_64".into()),
            ("engine_backend".into(), "kvm".into()),
            ("engine_timebase".into(), "counter-pv-v1".into()),
        ]);
        for (kind, clock) in [
            (ExecutableKind::Standalone, None),
            (ExecutableKind::Standalone, Some("counter")),
            (ExecutableKind::Standalone, Some("pv")),
            (ExecutableKind::InternalWorker, Some("counter")),
            (ExecutableKind::InternalWorker, Some("pv")),
            (ExecutableKind::ArmWorld, Some("pv")),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let engine = directory.path().join("retained-engine");
            fs::write(&engine, b"never execute this fixture").unwrap();
            let hash = ContentDigest::of(b"retained input");
            let mut semantic_parameters = match kind {
                ExecutableKind::Standalone => BTreeMap::new(),
                ExecutableKind::InternalWorker => internal.clone(),
                ExecutableKind::ArmWorld => crate::engine::arm_parameters(),
            };
            if let Some(clock) = clock {
                semantic_parameters.insert("timebase".into(), clock.into());
            }
            semantic_parameters.insert("memory_mb".into(), "512".into());
            let prepared = PreparedInputs {
                kernel: hash.clone(),
                images: BTreeMap::new(),
                placement: BTreeMap::from([
                    ("client".into(), "client".into()),
                    ("server".into(), "server".into()),
                ]),
                guests: BTreeMap::from([
                    ("client".into(), hash.clone()),
                    ("server".into(), hash.clone()),
                ]),
                profile: ExecutionProfile {
                    backend: if kind == ExecutableKind::ArmWorld {
                        "seedbox-hvf"
                    } else {
                        "seedbox-kvm"
                    }
                    .into(),
                    engine: hash.clone(),
                    witness_mode: WitnessMode::FullState,
                    semantic_parameters,
                },
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            };
            let artifacts = BTreeMap::from([(hash, engine.clone())]);
            let request = ExecutionRequest {
                seed: 1009,
                limits: &Limits {
                    simulated_time_ns: 1_000_000_000,
                    wall_time_ns: 5_000_000_000,
                },
                network: &Network {
                    links: vec![],
                    name: "private".into(),
                    latency_ns: 100_000,
                    jitter_ns: 20_000,
                    bandwidth_bps: None,
                    queue_bound_ns: None,
                },
                schedule: &Schedule {
                    timer_jitter_ns: 50_000,
                    stop_tolerance_ns: 40_000,
                },
                control: None,
                standby: &[],
                check: &Check {
                    service: "client".into(),
                    exit_code: 0,
                },
                prepared: &prepared,
                artifacts: &artifacts,
                output: directory.path(),
                cancellation: &CancellationToken::default(),
                performance_parameters: &BTreeMap::new(),
            };
            let original = serde_json::to_vec(&prepared.profile).unwrap();
            let launch = WorkerLaunch::prepare(&request).unwrap();
            let args: Vec<_> = launch
                .command
                .get_args()
                .map(|v| v.to_str().unwrap())
                .collect();
            assert_eq!(launch.command.get_program(), engine.as_os_str());
            let dispatch: &[&str] = match kind {
                ExecutableKind::Standalone => &[],
                ExecutableKind::InternalWorker => &["__engine", crate::engine::INTERNAL_WORKER_V1],
                ExecutableKind::ArmWorld => &["__engine", "arm64-world-v1"],
            };
            assert_eq!(&args[..dispatch.len()], dispatch);
            let option = |flag| {
                args.iter()
                    .position(|arg| *arg == flag)
                    .map(|i| args[i + 1])
            };
            assert_eq!(
                option("--timebase"),
                if kind != ExecutableKind::ArmWorld && clock == Some("pv") {
                    Some("pv")
                } else {
                    None
                }
            );
            assert_eq!(
                option("--out"),
                Some(
                    directory
                        .path()
                        .join(crate::vmm::MACHINE_OUTPUT_DIR)
                        .to_str()
                        .unwrap()
                )
            );
            assert_eq!(option("--console-limit"), Some("268435456"));
            assert_eq!(
                option("--log-chunk-limit"),
                if kind == ExecutableKind::ArmWorld {
                    None
                } else {
                    Some("65536")
                }
            );
            assert_eq!(
                args.contains(&"--stream-console"),
                kind != ExecutableKind::ArmWorld
            );
            assert!(args.contains(&"--digest"));
            assert_eq!(
                launch.timebase,
                if kind == ExecutableKind::ArmWorld || clock == Some("pv") {
                    Timebase::Pv
                } else {
                    Timebase::Counter
                }
            );
            assert_eq!(serde_json::to_vec(&prepared.profile).unwrap(), original);
            drop(launch);
            assert!(!directory.path().join("host-control").exists());
        }
    }

    fn worker(output: &std::path::Path, script: &str) -> RunningWorker {
        let report = output.join("vmm.json");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .env_clear()
            .process_group(0)
            .stdout(fs::File::create(&report).unwrap());
        WorkerLaunch {
            command,
            engine: "/bin/sh".into(),
            report,
            endpoint: Endpoint::new(output).unwrap(),
            spawn_fix: "test worker",
            timebase: Timebase::Counter,
        }
        .spawn()
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
    fn observing_exit_preserves_ownership_until_cleanup() {
        let output = tempfile::tempdir().unwrap();
        let mut child = worker(output.path(), "printf result");
        let pid = child.process.0.as_ref().unwrap().id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.process.exited().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
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
            let child = worker(output.path(), "exec /bin/sleep 60");
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
    fn report_errors_and_early_drop_clean_up_the_worker_and_endpoint() {
        for early_drop in [false, true] {
            let output = tempfile::tempdir().unwrap();
            let child = worker(
                output.path(),
                if early_drop {
                    "exec /bin/sleep 60"
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
