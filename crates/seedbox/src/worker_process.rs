//! Shared ownership of a retained engine process and its execution output.
//! Platform adapters hold host guards outside this lifetime and supply only
//! backend arguments, admission and report validation.

use crate::{
    engine::ExecutableKind,
    execution::{CancellationToken, ExecutionReport, ExecutionRequest, StopReason, WitnessMode},
    host_control::Endpoint,
    Error, Result,
};
use seedbox_engine_core::launch::{Performance, WorkerRequest};
use std::{
    collections::BTreeMap,
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

pub(crate) fn incomplete(reason: StopReason) -> ExecutionReport {
    ExecutionReport {
        reason,
        workload_exit: None,
        witness: None,
        full_state: None,
        faults: Vec::new(),
        detail: None,
    }
}

/// The x86 worker's performance settings from a run's recorded performance
/// parameters (`SEEDBOX_WATCHDOG_MS`); absent ones take defaults.
pub(crate) fn launch_performance(parameters: &BTreeMap<String, String>) -> Result<Performance> {
    let mut performance = Performance::default();
    for (key, value) in parameters {
        let invalid = || Error::Integrity(format!("invalid performance parameter {key}={value}"));
        match key.as_str() {
            "SEEDBOX_WATCHDOG_MS" => {
                performance.watchdog_ms = value
                    .parse()
                    .ok()
                    .filter(|ms| *ms != 0)
                    .ok_or_else(invalid)?
            }
            _ => return Err(invalid()),
        }
    }
    Ok(performance)
}

pub(crate) struct WorkerLaunch {
    pub(crate) command: Command,
    engine: PathBuf,
    report: PathBuf,
    endpoint: Endpoint,
    spawn_fix: &'static str,
}

/// Launch request beside `world.json` in the run output; the x86 worker reads it.
pub(crate) const WORKER_REQUEST_FILE: &str = "worker-request.json";

impl WorkerLaunch {
    /// `fork_seeds` are released after the request's own seed from the same
    /// booted world; empty for an ordinary run.
    pub(crate) fn prepare(request: &ExecutionRequest<'_>, fork_seeds: &[u64]) -> Result<Self> {
        let engine = request
            .artifacts
            .get(&request.prepared.profile.engine)
            .ok_or_else(|| Error::Integrity("missing retained engine executable".into()))?;
        // A shared exploration copy is already 0500; chmod would change its
        // ctime and make the store replace it for every attempt.
        if fs::metadata(engine)?.permissions().mode() & 0o7777 != 0o500 {
            fs::set_permissions(engine, fs::Permissions::from_mode(0o500))?;
        }
        let world = request.output.join("world.json");
        fs::write(
            &world,
            serde_json::to_vec_pretty(&crate::engine_adapter::world(request)?)?,
        )?;
        let report = request.output.join("vmm.json");
        let endpoint = Endpoint::new(request.output)?;
        let kind = crate::engine::recorded_launch(&request.prepared.profile)?;
        let mut command = crate::engine_adapter::engine_command(engine, &request.prepared.profile)?;
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
        let out = request
            .output
            .join(crate::engine_adapter::MACHINE_OUTPUT_DIR);
        let console_limit = 1024 * 1024 * 1024 / streams;
        let digest = request.prepared.profile.witness_mode == WitnessMode::FullState;
        if kind == ExecutableKind::ArmWorld {
            // The ARM world keeps its own flag contract.
            command
                .arg("--config")
                .arg(&world)
                .arg("--seed")
                .arg(request.seed.to_string())
                .arg("--out")
                .arg(&out)
                .arg("--host-control")
                .arg(endpoint.socket())
                .arg("--console-limit")
                .arg(console_limit.to_string());
            if digest {
                command.arg("--digest");
            }
        } else {
            let mut worker = WorkerRequest::new(world, request.seed, out);
            worker.fork_seeds = fork_seeds.to_vec();
            worker.digest = digest;
            worker.host_control = Some(endpoint.socket());
            worker.console_limit = Some(console_limit);
            // Bound observation metadata to at most 40 MiB across all streams.
            worker.log_chunk_limit = Some((1_048_576 / streams).min(65_536));
            worker.stream_console = true;
            worker.features = crate::engine_adapter::recorded_features(&request.prepared.profile)?;
            worker.performance = launch_performance(request.performance_parameters)?;
            let path = request.output.join(WORKER_REQUEST_FILE);
            fs::write(&path, serde_json::to_vec_pretty(&worker)?)?;
            command.arg("--request").arg(path);
        }
        command
            .env_clear()
            .stdout(fs::File::create(&report)?)
            .stderr(fs::File::create(request.output.join("vmm.stderr"))?)
            .process_group(0);
        Ok(Self {
            command,
            engine: engine.clone(),
            report,
            endpoint,
            spawn_fix,
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
            _endpoint: self.endpoint,
        })
    }
}

pub(crate) struct RunningWorker {
    // Field drop order keeps process cleanup ahead of endpoint removal.
    process: OwnedProcess,
    report: PathBuf,
    started: Instant,
    _endpoint: Endpoint,
}

impl RunningWorker {
    /// `parse_report` reads a finished worker's report; `stopped` turns the
    /// cancellation or wall-time report into the same result type.
    pub(crate) fn wait<T>(
        mut self,
        cancellation: &CancellationToken,
        wall_time_ns: u64,
        parse_report: impl FnOnce(&[u8]) -> Result<T>,
        stopped: impl FnOnce(ExecutionReport) -> T,
    ) -> Result<T> {
        let completion = (|| loop {
            if cancellation.is_cancelled() {
                return Ok(stopped(incomplete(StopReason::Cancelled)));
            }
            if let Some(success) = self.process.exited()? {
                return if success {
                    parse_report(&fs::read(&self.report)?)
                } else {
                    Err(Error::Execution(
                        crate::engine_adapter::VMM_FAILURE_MESSAGE.into(),
                    ))
                };
            }
            if self.started.elapsed() >= Duration::from_nanos(wall_time_ns) {
                return Ok(stopped(incomplete(StopReason::WallTimeLimit)));
            }
            std::thread::sleep(Duration::from_millis(10));
        })();
        // Cleanup failure takes precedence over any apparent result. Drop
        // retries for about a second before reporting a failure.
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
    fn prepared_launch_uses_recorded_dispatch_and_retention_limits() {
        use crate::{
            config::{Check, Limits, Network, Schedule},
            execution::{Backend, ExecutionProfile, PreparedInputs, SemanticParameters},
            store::ContentDigest,
        };
        use std::collections::BTreeMap;
        let internal =
            crate::engine::kvm_descriptor(ExecutableKind::InternalWorker, "0.1.0-dev.1".into());
        for kind in [
            ExecutableKind::Standalone,
            ExecutableKind::InternalWorker,
            ExecutableKind::ArmWorld,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let engine = directory.path().join("retained-engine");
            fs::write(&engine, b"never execute this fixture").unwrap();
            let hash = ContentDigest::of(b"retained input");
            let mut semantic_parameters = SemanticParameters::new(512);
            semantic_parameters.engine = match kind {
                ExecutableKind::Standalone => None,
                ExecutableKind::InternalWorker => Some(internal.clone()),
                ExecutableKind::ArmWorld => Some(crate::engine::arm_descriptor()),
            };
            let prepared = PreparedInputs {
                wall_clock_epoch: None,
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
                        Backend::Hvf
                    } else {
                        Backend::Kvm
                    },
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
                    start_stagger_ns: 0,
                },
                control: None,
                check: &Check {
                    service: "client".into(),
                    exit_code: 0,
                },
                prepared: &prepared,
                artifacts: &artifacts,
                output: directory.path(),
                cancellation: &CancellationToken::default(),
                performance_parameters: &BTreeMap::from([(
                    "SEEDBOX_WATCHDOG_MS".into(),
                    "50".into(),
                )]),
            };
            let original = serde_json::to_vec(&prepared.profile).unwrap();
            let launch = WorkerLaunch::prepare(&request, &[]).unwrap();
            let args: Vec<_> = launch
                .command
                .get_args()
                .map(|v| v.to_str().unwrap())
                .collect();
            assert_eq!(launch.command.get_program(), engine.as_os_str());
            let dispatch: &[&str] = match kind {
                ExecutableKind::Standalone => &[],
                ExecutableKind::InternalWorker => &["__engine", crate::engine::INTERNAL_WORKER_V1],
                ExecutableKind::ArmWorld => &["__engine", crate::engine::ARM_WORLD_V1],
            };
            assert_eq!(&args[..dispatch.len()], dispatch);
            let out = directory
                .path()
                .join(crate::engine_adapter::MACHINE_OUTPUT_DIR);
            if kind == ExecutableKind::ArmWorld {
                let world = directory.path().join("world.json");
                let socket = launch.endpoint.socket();
                let expected = [
                    "--config",
                    world.to_str().unwrap(),
                    "--seed",
                    "1009",
                    "--out",
                    out.to_str().unwrap(),
                    "--host-control",
                    socket.to_str().unwrap(),
                    "--console-limit",
                    "268435456",
                    "--digest",
                ];
                assert_eq!(&args[dispatch.len()..], expected);
            } else {
                let path = directory.path().join(WORKER_REQUEST_FILE);
                assert_eq!(
                    &args[dispatch.len()..],
                    ["--request", path.to_str().unwrap()]
                );
                let worker: WorkerRequest =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                let mut expected =
                    WorkerRequest::new(directory.path().join("world.json"), 1009, out);
                expected.digest = true;
                expected.host_control = Some(launch.endpoint.socket());
                expected.console_limit = Some(268435456);
                expected.log_chunk_limit = Some(65536);
                expected.stream_console = true;
                expected.performance.watchdog_ms = 50;
                assert_eq!(worker, expected);
            }
            assert_eq!(serde_json::to_vec(&prepared.profile).unwrap(), original);
            drop(launch);
            assert!(!directory.path().join("host-control").exists());
        }
    }

    #[test]
    fn recorded_performance_parameters_decode_strictly() {
        let decode = |pairs: &[(&str, &str)]| {
            launch_performance(
                &pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        };
        assert_eq!(decode(&[]).unwrap(), Performance::default());
        assert_eq!(
            decode(&[("SEEDBOX_WATCHDOG_MS", "50")]).unwrap(),
            Performance { watchdog_ms: 50 }
        );
        for bad in [
            ("SEEDBOX_WATCHDOG_MS", "0"),
            ("SEEDBOX_WATCHDOG_MS", "fast"),
            ("SEEDBOX_MARGIN", "64"),
        ] {
            assert!(decode(&[bad]).is_err(), "{bad:?}");
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
                .wait(
                    &token,
                    1,
                    |_| -> Result<ExecutionReport> {
                        panic!("unfinished worker report must not be read")
                    },
                    |stopped| stopped,
                )
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
                    .wait(
                        &CancellationToken::default(),
                        5_000_000_000,
                        |_| -> Result<ExecutionReport> {
                            Err(Error::Integrity("invalid test report".into()))
                        },
                        |stopped| stopped,
                    )
                    .unwrap_err();
                assert!(error.to_string().contains("invalid test report"));
            }
            assert_reaped(pid, output.path());
        }
    }
}
