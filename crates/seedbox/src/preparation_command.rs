//! Owned preparation commands and bounded caller-owned evidence. No guest work.
use crate::{execution::CancellationToken, store::ContentDigest, Error, Result};
use serde::Serialize;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

// A fork can inherit another unit test's lease descriptor until exec. These
// process controls exclude the immediate-close lease assertion, without changing
// production leases or the integration tests' explicit concurrent executions.
#[cfg(test)]
pub(crate) static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

const FILE_LIMIT: u64 = 4 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 64 * 1024 * 1024;
const METADATA_LIMIT: u64 = 4 * 1024 * 1024;
const VERSION_LIMIT: u64 = 8192;
const VERSION_TOTAL_LIMIT: u64 = 4 * 1024 * 1024;

/// Pure inspection shared with opt-in timestamp policy and per-call reuse.
/// Admission receipts and cancellable local-mode checks remain context-owned.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct NativeBuilder {
    pub launcher: PathBuf,
    pub native: PathBuf,
    pub launch_hash: ContentDigest,
    pub native_hash: Option<ContentDigest>,
}
pub(crate) fn inspect_native_builder(requested: &Path) -> Result<NativeBuilder> {
    let launcher = fs::canonicalize(requested).map_err(|e| {
        Error::Setup(format!(
            "builder {}: {e}; check the builder path or enter nix develop",
            requested.display()
        ))
    })?;
    let native = launcher.with_file_name(".podman-wrapped");
    let launch_hash = ContentDigest::of(&fs::read(&launcher).map_err(|e| {
        Error::Setup(format!(
            "builder {}: {e}; check the builder path or enter nix develop",
            requested.display()
        ))
    })?);
    let native_hash = fs::read(&native).map(|b| ContentDigest::of(&b)).ok();
    Ok(NativeBuilder {
        launcher,
        native,
        launch_hash,
        native_hash,
    })
}

/// Caller-owned preparation evidence and the shared cancellation token.
#[derive(Debug)]
pub struct PreparationContext {
    pub cancellation: CancellationToken,
    pub directory: PathBuf,
    quotas: Mutex<(u64, u64, u64)>,
    admitted_builder: Mutex<Option<PathBuf>>,
    cleanup_timeout: Duration,
}
#[derive(Serialize)]
struct Receipt {
    phase: String,
    program: PathBuf,
    resolved_program: PathBuf,
    sha256: ContentDigest,
    argv: Vec<String>,
    cwd: Option<PathBuf>,
    status: String,
    private_tag: Option<String>,
    local_mode: bool,
    cancelled: bool,
    timed_out: bool,
    escalated: bool,
    reaped: bool,
    cleanup: String,
    elapsed_seconds: f64,
    retained_bytes: u64,
    omitted_bytes: u64,
    omission_reason: Option<String>,
    output_complete: bool,
    pid: Option<u32>,
    required_stdout_retained_bytes: Option<u64>,
    required_stdout_omitted_bytes: Option<u64>,
}
impl PreparationContext {
    pub fn new(directory: impl Into<PathBuf>, cancellation: CancellationToken) -> Result<Self> {
        let directory = directory.into();
        fs::create_dir(&directory).map_err(|e| {
            Error::Setup(format!("preparation evidence {}: {e}", directory.display()))
        })?;
        Ok(Self {
            cancellation,
            directory,
            quotas: Mutex::new((0, 0, 0)),
            admitted_builder: Mutex::new(None),
            cleanup_timeout: CLEANUP_TIMEOUT,
        })
    }
    pub fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            Err(Error::PreparationCancelled)
        } else {
            Ok(())
        }
    }
    fn metadata(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let result = (|| -> Result<()> {
            let bytes = serde_json::to_vec_pretty(value)?;
            let mut quota = self.quotas.lock().map_err(|_| {
                Error::PreparationCleanupUnverified(
                    "required preparation evidence lock poisoned".into(),
                )
            })?;
            if quota.1.saturating_add(bytes.len() as u64) > METADATA_LIMIT {
                return Err(Error::Execution(
                    "preparation metadata quota exhausted; cleanup evidence unavailable".into(),
                ));
            }
            let mut temp = tempfile::NamedTempFile::new_in(&self.directory)?;
            temp.write_all(&bytes)?;
            temp.as_file().sync_all()?;
            temp.persist(self.directory.join(name))
                .map_err(|e| e.error)?;
            quota.1 += bytes.len() as u64;
            Ok(())
        })();
        result.map_err(|error| {
            Error::PreparationCleanupUnverified(format!(
                "required preparation evidence {name}: {error}"
            ))
        })
    }
    /// Inspect the builder and check local Podman operation before exploration.
    pub(crate) fn admit_builder(&self, requested: &Path) -> Result<()> {
        let NativeBuilder {
            launcher,
            native,
            launch_hash,
            native_hash,
        } = inspect_native_builder(requested)?;
        self.metadata("builder.json", &serde_json::json!({"requested": requested, "launcher": launcher, "launcher_sha256": launch_hash, "native": native, "native_sha256": native_hash, "local_mode": true, "cancellation_cleanup_qualified": false}))?;
        *self.admitted_builder.lock().map_err(|_| {
            Error::PreparationCleanupUnverified("builder admission lock poisoned".into())
        })? = Some(launcher);
        self.run(
            Command::new(requested).args([
                "--remote=false",
                "info",
                "--format",
                "{{json .Store}} {{json .Host.OCIRuntime}}",
            ]),
            "builder-local-info",
            None,
            false,
            false,
        )?;
        Ok(())
    }
    /// Retain the successful source-group owner for every service, including reuse.
    pub(crate) fn source_group(
        &self,
        service: &str,
        owner: &str,
        context: &ContentDigest,
        dockerfile: &ContentDigest,
        manifest: &ContentDigest,
        epoch: Option<u32>,
    ) -> Result<()> {
        self.check()?;
        self.metadata(
            &format!("{service}-source-group.json"),
            &serde_json::json!({
                "service":service,"owner":owner,"context":context,"dockerfile":dockerfile,
                "manifest":manifest,"build_epoch":epoch,"reused":service != owner
            }),
        )
    }
    pub(crate) fn setup_inputs(
        &self,
        auxiliary: &std::collections::BTreeMap<String, ContentDigest>,
        engine: &ContentDigest,
        kernel: &ContentDigest,
    ) -> Result<()> {
        self.metadata(
            "setup.json",
            &serde_json::json!({"auxiliary":auxiliary,"engine":engine,"kernel":kernel}),
        )
    }
    pub(crate) fn build_identity(
        &self,
        phase: &str,
        tag: &str,
        context: &ContentDigest,
        dockerfile: &ContentDigest,
    ) -> Result<()> {
        self.metadata(&format!("{phase}-build-identity.json"), &serde_json::json!({"private_tag": tag, "context": context, "dockerfile": dockerfile, "cleanup": "unfinished"}))
    }
    pub(crate) fn run(
        &self,
        command: &mut Command,
        phase: &str,
        tag: Option<&str>,
        build: bool,
        cleanup: bool,
    ) -> Result<()> {
        if !cleanup {
            self.check()?;
        }
        self.run_owned(command, phase, tag, build, cleanup, None)
    }
    /// Capture a small protocol reply with the same cancellation, quotas and
    /// process ownership as preparation commands. Never use unbounded output().
    pub(crate) fn output(
        &self,
        command: &mut Command,
        phase: &str,
        cleanup: bool,
    ) -> Result<Vec<u8>> {
        if !cleanup {
            self.check()?;
        }
        let mut output = Vec::new();
        self.run_owned(command, phase, None, false, cleanup, Some(&mut output))?;
        Ok(output)
    }
    #[cfg(unix)]
    fn run_owned(
        &self,
        command: &mut Command,
        phase: &str,
        tag: Option<&str>,
        build: bool,
        cleanup: bool,
        stdout_value: Option<&mut Vec<u8>>,
    ) -> Result<()> {
        use std::os::unix::{net::UnixStream, process::CommandExt};
        let program = PathBuf::from(command.get_program());
        let resolved = fs::canonicalize(&program)
            .map_err(|e| crate::oci::spawn_error(program.as_os_str(), e))?;
        if tag.is_some() {
            if let Some(admitted) = self
                .admitted_builder
                .lock()
                .map_err(|_| {
                    Error::PreparationCleanupUnverified("builder admission lock poisoned".into())
                })?
                .as_ref()
            {
                if admitted != &resolved {
                    return Err(Error::PreparationCleanupUnverified(
                        "builder launch path changed after admission; private tag retained".into(),
                    ));
                }
            }
        }
        let mut receipt = Receipt {
            phase: phase.into(),
            program,
            resolved_program: resolved.clone(),
            sha256: ContentDigest::of(&fs::read(resolved)?),
            argv: command
                .get_args()
                .map(|s| s.to_string_lossy().into_owned())
                .collect(),
            cwd: Some(
                command
                    .get_current_dir()
                    .map(Path::to_owned)
                    .unwrap_or(std::env::current_dir()?),
            ),
            status: "unfinished".into(),
            private_tag: tag.map(str::to_owned),
            local_mode: command.get_args().any(|arg| arg == "--remote=false"),
            cancelled: false,
            timed_out: false,
            escalated: false,
            reaped: false,
            cleanup: "unfinished".into(),
            elapsed_seconds: 0.0,
            retained_bytes: 0,
            omitted_bytes: 0,
            omission_reason: None,
            output_complete: false,
            pid: None,
            required_stdout_retained_bytes: None,
            required_stdout_omitted_bytes: None,
        };
        let metadata = format!("{phase}.json");
        self.metadata(&metadata, &receipt)?;
        let quota = self.quotas.lock().map_err(|_| {
            Error::PreparationCleanupUnverified(
                "required preparation evidence lock poisoned".into(),
            )
        })?;
        let phase_limit = if phase.ends_with("version") {
            8192
        } else {
            FILE_LIMIT
        };
        let remaining = phase_limit.min(TOTAL_LIMIT.saturating_sub(quota.0));
        let version_remaining = VERSION_LIMIT.min(VERSION_TOTAL_LIMIT.saturating_sub(quota.2));
        drop(quota);
        let log = self.directory.join(format!("{phase}.log"));
        let mut file = fs::File::create(&log).map_err(|error| {
            Error::PreparationCleanupUnverified(format!(
                "required preparation log {}: {error}",
                log.display()
            ))
        })?;
        let (mut reader, writer) = UnixStream::pair()?;
        reader.set_read_timeout(Some(Duration::from_millis(50)))?;
        // Required stdout is independent of stderr and the diagnostic quota.
        let required_stream = if stdout_value.is_some() {
            let (required_reader, required_writer) = UnixStream::pair()?;
            required_reader.set_read_timeout(Some(Duration::from_millis(50)))?;
            command.stdout(Stdio::from(std::os::fd::OwnedFd::from(required_writer)));
            Some(required_reader)
        } else {
            command.stdout(Stdio::from(std::os::fd::OwnedFd::from(writer.try_clone()?)));
            None
        };
        command
            .process_group(0)
            .stderr(Stdio::from(std::os::fd::OwnedFd::from(writer)));
        let began = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                receipt.status = e.to_string();
                receipt.cleanup = "not-started".into();
                self.metadata(&metadata, &receipt)?;
                return Err(crate::oci::spawn_error(command.get_program(), e));
            }
        };
        receipt.pid = Some(child.id());
        // Drop parent copies of the command's pipe handles before draining.
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let finish = Arc::new(AtomicBool::new(false));
        let done = finish.clone();
        #[cfg(test)]
        let test_timeout_window =
            (phase == "test-timeout-window").then(|| self.directory.join("release-child"));
        let drain = std::thread::spawn(move || -> std::io::Result<(u64, u64, bool)> {
            let mut seen = 0u64;
            let mut retained = 0u64;
            let mut bytes = [0u8; 8192];
            let mut complete = false;
            let mut finishing = None;
            loop {
                if done.load(Ordering::Acquire) {
                    let at = finishing.get_or_insert_with(Instant::now);
                    if at.elapsed() > Duration::from_millis(200) {
                        break;
                    }
                }
                match reader.read(&mut bytes) {
                    Ok(0) => {
                        complete = true;
                        break;
                    }
                    Ok(n) => {
                        seen = seen.saturating_add(n as u64);
                        let keep = (remaining - retained).min(n as u64) as usize;
                        file.write_all(&bytes[..keep])?;
                        retained += keep as u64;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        #[cfg(test)]
                        if let Some(release) = &test_timeout_window {
                            // Force cleanup to finish after read timed out, before its result is observed.
                            fs::write(release, b"exit now")?;
                            let deadline = Instant::now() + Duration::from_secs(3);
                            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }
                        // Retry until EOF or the bounded post-finish drain deadline.
                    }
                    Err(e) => return Err(e),
                }
            }
            file.sync_all()?;
            Ok((retained, seen - retained, complete))
        });
        let required_done = finish.clone();
        #[cfg(test)]
        let test_required_timeout_window =
            (phase == "test-timeout-stdout").then(|| self.directory.join("release-child"));
        let required_drain = required_stream.map(|mut reader| {
            std::thread::spawn(move || -> std::io::Result<(Vec<u8>, u64, bool)> {
                let mut value = Vec::new();
                let mut seen = 0u64;
                let mut bytes = [0u8; 8192];
                let mut complete = false;
                let mut finishing = None;
                loop {
                    if required_done.load(Ordering::Acquire) {
                        let at = finishing.get_or_insert_with(Instant::now);
                        if at.elapsed() > Duration::from_millis(200) {
                            break;
                        }
                    }
                    match reader.read(&mut bytes) {
                        Ok(0) => {
                            complete = true;
                            break;
                        }
                        Ok(n) => {
                            seen = seen.saturating_add(n as u64);
                            let keep =
                                (version_remaining - value.len() as u64).min(n as u64) as usize;
                            value.extend_from_slice(&bytes[..keep]);
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            #[cfg(test)]
                            if let Some(release) = &test_required_timeout_window {
                                // Force cleanup to finish after read timed out, before its result is observed.
                                fs::write(release, b"exit now")?;
                                let deadline = Instant::now() + Duration::from_secs(3);
                                while !required_done.load(Ordering::Acquire)
                                    && Instant::now() < deadline
                                {
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                            }
                            // Retry until EOF or the bounded post-finish drain deadline.
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok((value, seen, complete))
            })
        });
        let pid = child.id() as i32;
        let mut termination_at = None;
        let status = loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let waited = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if waited != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                // ECHILD means ownership was lost to an external reaper: never
                // signal a numeric identity which might already have been reused.
                if error.raw_os_error() != Some(libc::ECHILD) {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    };
                }
                break Err(error);
            }
            if unsafe { info.si_pid() } != 0 {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                };
                break child.wait();
            }
            if !cleanup && self.cancellation.is_cancelled() && termination_at.is_none() {
                receipt.cancelled = true;
                termination_at = Some(Instant::now());
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                };
            }
            // Container removal can perform slow daemon-side filesystem work.
            // Give that operation its own deadline, then use the same short
            // TERM/KILL/reap sequence as caller cancellation.
            if cleanup && began.elapsed() >= self.cleanup_timeout && termination_at.is_none() {
                receipt.timed_out = true;
                termination_at = Some(Instant::now());
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                };
            }
            if !receipt.escalated
                && termination_at.is_some_and(|at| at.elapsed() >= TERMINATION_GRACE)
            {
                receipt.escalated = true;
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                };
            }
            if termination_at.is_some_and(|at| at.elapsed() >= REAP_TIMEOUT) {
                break Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "owned leader did not exit after escalation",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        finish.store(true, Ordering::Release);
        receipt.reaped = status.is_ok();
        receipt.elapsed_seconds = began.elapsed().as_secs_f64();
        let output = drain.join().map_err(|_| {
            Error::PreparationCleanupUnverified(
                "required preparation output reader panicked".into(),
            )
        })?;
        if let Ok((retained, omitted, complete)) = &output {
            receipt.output_complete = *complete;
            receipt.retained_bytes = *retained;
            receipt.omitted_bytes = *omitted;
            self.quotas
                .lock()
                .map_err(|_| {
                    Error::PreparationCleanupUnverified(
                        "required preparation evidence lock poisoned".into(),
                    )
                })?
                .0 += *retained;
            if *omitted > 0 {
                receipt.omission_reason = Some("phase or total payload quota".into());
            }
        }
        let required_output = required_drain
            .map(|drain| {
                drain
                    .join()
                    .map_err(|_| {
                        Error::PreparationCleanupUnverified(
                            "required stdout reader panicked".into(),
                        )
                    })?
                    .map_err(|e| {
                        Error::PreparationCleanupUnverified(format!("required stdout {phase}: {e}"))
                    })
            })
            .transpose()?;
        if let Some((value, seen, complete)) = &required_output {
            // This bounded raw stdout survives even if diagnostic payload is exhausted.
            fs::write(self.directory.join(format!("{phase}.stdout")), value).map_err(|e| {
                Error::PreparationCleanupUnverified(format!(
                    "required stdout evidence {phase}: {e}"
                ))
            })?;
            receipt.required_stdout_retained_bytes = Some(value.len() as u64);
            receipt.required_stdout_omitted_bytes = Some(seen - value.len() as u64);
            receipt.output_complete &= complete;
            self.quotas
                .lock()
                .map_err(|_| {
                    Error::PreparationCleanupUnverified(
                        "required stdout evidence lock poisoned".into(),
                    )
                })?
                .2 += value.len() as u64;
        }
        receipt.status = match &status {
            Ok(status) => status.to_string(),
            Err(e) => e.to_string(),
        };
        receipt.cleanup = if build && receipt.cancelled {
            "cleanup-unverified; private tag retained"
        } else if receipt.reaped {
            "owned group signalled and leader reaped"
        } else {
            "cleanup-unverified"
        }
        .into();
        if !receipt.output_complete {
            receipt.omission_reason=Some("output stream remained open after owned process cleanup; further byte count unknown".into());
            receipt.cleanup = "cleanup-unverified".into();
        }
        self.metadata(&metadata, &receipt)?;
        output.map_err(|error| {
            Error::PreparationCleanupUnverified(format!(
                "required preparation output {phase}: {error}"
            ))
        })?;
        if !receipt.output_complete {
            return Err(Error::PreparationCleanupUnverified(format!(
                "{phase}: output stream remained open after cleanup; private tag retained"
            )));
        }
        if receipt.timed_out {
            return Err(Error::PreparationCleanupUnverified(format!(
                "{phase}: cleanup exceeded its {}-second deadline; daemon removal must be verified",
                self.cleanup_timeout.as_secs_f64()
            )));
        }
        let status = status.map_err(|e| {
            Error::PreparationCleanupUnverified(format!(
                "command {}: wait/reap ownership uncertain: {e}",
                receipt.program.display()
            ))
        })?;
        if build && receipt.cancelled {
            return Err(Error::PreparationCleanupUnverified(format!(
                "{} cancelled; private tag {} retained",
                phase,
                tag.unwrap_or("unknown")
            )));
        }
        if receipt.cancelled {
            return Err(Error::PreparationCancelled);
        }
        if !status.success() {
            let tail = fs::read(log)?;
            return Err(Error::Execution(format!(
                "command {} {:?} failed ({status}): {}",
                receipt.program.display(),
                receipt.argv,
                String::from_utf8_lossy(&tail[..tail.len().min(8192)])
            )));
        }
        if let Some((value, seen, _)) = required_output {
            if seen > value.len() as u64 {
                return Err(Error::Execution(format!("{phase}: required stdout version exceeds 8192-byte phase or 4-MiB total bound; PreparedInputs not accepted")));
            }
            if let Some(destination) = stdout_value {
                *destination = value;
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fn run_owned(
        &self,
        _command: &mut Command,
        _phase: &str,
        _tag: Option<&str>,
        _build: bool,
        _cleanup: bool,
        _stdout_value: Option<&mut Vec<u8>>,
    ) -> Result<()> {
        Err(Error::Setup(
            "exploration preparation requires Unix owned process groups".into(),
        ))
    }
    pub(crate) fn version(&self, tool: &Path, phase: &str) -> Result<String> {
        self.check()?;
        let mut bytes = Vec::new();
        self.run_owned(
            Command::new(tool).arg("--version"),
            phase,
            None,
            false,
            false,
            Some(&mut bytes),
        )?;
        Ok(String::from_utf8_lossy(&bytes).trim().into())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[test]
    fn slow_cleanup_can_finish_without_being_mistaken_for_a_hung_process() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let context =
            PreparationContext::new(root.path().join("evidence"), Default::default()).unwrap();
        let cleanup = script(root.path(), "cleanup", "sleep 6");
        context
            .run(
                &mut Command::new(cleanup),
                "slow-cleanup",
                None,
                false,
                true,
            )
            .unwrap();
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(context.directory.join("slow-cleanup.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["timed_out"], false);
        assert_eq!(receipt["escalated"], false);
        assert_eq!(receipt["reaped"], true);
    }
    #[test]
    fn cleanup_deadline_escalates_and_reaps_without_claiming_daemon_completion() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut context =
            PreparationContext::new(root.path().join("evidence"), Default::default()).unwrap();
        context.cleanup_timeout = Duration::from_millis(100);
        let cleanup = script(root.path(), "cleanup", "exec sleep 30");
        let mut command = Command::new(cleanup);
        // Install before exec so even a slow process start cannot race TERM.
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
        let began = Instant::now();
        let result = context.run(&mut command, "hung-cleanup", None, false, true);
        assert!(matches!(
            result,
            Err(Error::PreparationCleanupUnverified(_))
        ));
        assert!(began.elapsed() < REAP_TIMEOUT);
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(context.directory.join("hung-cleanup.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["timed_out"], true);
        assert_eq!(receipt["cancelled"], false);
        assert_eq!(receipt["escalated"], true);
        assert_eq!(receipt["reaped"], true);
        assert_eq!(receipt["output_complete"], true);
    }
    #[test]
    fn required_version_stdout_ignores_warnings_and_diagnostic_quota() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let owned = tempfile::tempdir().unwrap();
        let tool = script(
            owned.path(),
            "tool",
            "printf 'stable-v1\\n'; printf '%s\\n' \"$VERSION_WARNING\" >&2",
        );
        let context =
            PreparationContext::new(owned.path().join("evidence"), CancellationToken::default())
                .unwrap();
        // Change the same executable's stderr using a caller-owned input file.
        fs::write(
            &tool,
            format!(
                "#!/bin/sh\nprintf 'stable-v1\\n'; cat '{}' >&2\n",
                owned.path().join("warning").display()
            ),
        )
        .unwrap();
        fs::write(owned.path().join("warning"), b"warning-one\n").unwrap();
        let first = context.version(&tool, "first-version").unwrap();
        fs::write(owned.path().join("warning"), b"different-warning\n").unwrap();
        let second = context.version(&tool, "second-version").unwrap();
        assert_eq!(first, "stable-v1");
        assert_eq!(first, second);
        assert_eq!(
            fs::read(context.directory.join("first-version.log")).unwrap(),
            b"warning-one\n"
        );
        assert_eq!(
            fs::read(context.directory.join("second-version.log")).unwrap(),
            b"different-warning\n"
        );
        // Exercise actual draining/accounting with only three payload bytes left.
        context.quotas.lock().unwrap().0 = TOTAL_LIMIT - 3;
        let exhaust = script(owned.path(), "exhaust", "printf 'abcdef'");
        context
            .run(&mut Command::new(exhaust), "exhaust", None, false, false)
            .unwrap();
        assert_eq!(context.quotas.lock().unwrap().0, TOTAL_LIMIT);
        let third = context.version(&tool, "third-version").unwrap();
        assert_eq!(third, first);
        let directory = context.directory.clone();
        drop(context);
        assert_eq!(
            fs::read(directory.join("third-version.stdout")).unwrap(),
            b"stable-v1\n"
        );
        assert!(fs::read(directory.join("third-version.log"))
            .unwrap()
            .is_empty());
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("third-version.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["omitted_bytes"], b"different-warning\n".len());
        assert_eq!(receipt["required_stdout_omitted_bytes"], 0);
        assert_eq!(
            receipt["required_stdout_retained_bytes"],
            b"stable-v1\n".len()
        );
    }
    #[test]
    fn required_version_bound_fails_instead_of_accepting_a_prefix() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let owned = tempfile::tempdir().unwrap();
        let tool = script(owned.path(), "tool", "printf '%08193d' 0");
        let context =
            PreparationContext::new(owned.path().join("evidence"), CancellationToken::default())
                .unwrap();
        let result = context.version(&tool, "large-version");
        assert!(
            matches!(result, Err(Error::Execution(ref error)) if error.contains("required stdout version exceeds"))
        );
        assert_eq!(
            fs::metadata(context.directory.join("large-version.stdout"))
                .unwrap()
                .len(),
            VERSION_LIMIT
        );
        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(context.directory.join("large-version.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["required_stdout_omitted_bytes"], 1);
        let small = script(owned.path(), "small", "printf 'abcd'");
        context.quotas.lock().unwrap().2 = VERSION_TOTAL_LIMIT - 3;
        assert!(matches!(
            context.version(&small, "total-version"),
            Err(Error::Execution(_))
        ));
        assert_eq!(
            fs::read(context.directory.join("total-version.stdout")).unwrap(),
            b"abc"
        );
    }
    #[test]
    fn bounded_output_is_drained_and_survives_scratch_and_context_drop() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let owned = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let path = script(
            scratch.path(),
            "output",
            "i=0; while [ $i -lt 5120 ]; do printf '%01024d' 0; i=$((i+1)); done",
        );
        let context =
            PreparationContext::new(owned.path().join("evidence"), CancellationToken::default())
                .unwrap();
        context
            .run(&mut Command::new(path), "output", None, false, false)
            .unwrap();
        drop(context);
        drop(scratch);
        let log = owned.path().join("evidence/output.log");
        assert_eq!(fs::metadata(log).unwrap().len(), FILE_LIMIT);
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(owned.path().join("evidence/output.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["omitted_bytes"], 1024 * 1024);
        assert_eq!(receipt["retained_bytes"], FILE_LIMIT);
        assert!(receipt["reaped"].as_bool().unwrap());
        assert!(receipt["omission_reason"].is_string());
    }
    #[test]
    fn cancellation_reaps_owned_group_keeps_sentinel_and_retains_arbitrary_bytes() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let child=script(directory.path(),"wait",&format!("trap '' TERM; printf 'arbitrary\\377diagnostics'; echo $$ > '{}'; while :; do :; done",ready.display()));
        let sentinel = script(directory.path(), "sentinel", "while :; do :; done");
        let mut unrelated = Command::new(sentinel).spawn().unwrap();
        let token = CancellationToken::default();
        let context =
            PreparationContext::new(directory.path().join("evidence"), token.clone()).unwrap();
        std::thread::scope(|scope| {
            let task =
                scope.spawn(|| context.run(&mut Command::new(child), "wait", None, false, false));
            let deadline = Instant::now() + Duration::from_secs(30);
            while fs::read_to_string(&ready)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .is_none()
            {
                let within_deadline = Instant::now() < deadline;
                if !within_deadline {
                    token.cancel();
                }
                assert!(within_deadline);
                std::thread::yield_now();
            }
            let pid: libc::pid_t = fs::read_to_string(&ready).unwrap().trim().parse().unwrap();
            token.cancel();
            assert!(matches!(
                task.join().unwrap(),
                Err(Error::PreparationCancelled)
            ));
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert!(unrelated.try_wait().unwrap().is_none());
        });
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        drop(context);
        assert!(fs::read(directory.path().join("evidence/wait.log"))
            .unwrap()
            .contains(&255));
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("evidence/wait.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["cancelled"], true);
        assert_eq!(receipt["escalated"], true);
        assert_eq!(receipt["reaped"], true);
    }
    #[test]
    fn cancelled_builder_is_unverified_and_does_not_claim_tag_cleanup() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let child = script(
            directory.path(),
            "build",
            &format!("echo $$ > '{}'; while :; do :; done", ready.display()),
        );
        let token = CancellationToken::default();
        let context =
            PreparationContext::new(directory.path().join("evidence"), token.clone()).unwrap();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| {
                context.run(
                    &mut Command::new(child),
                    "build",
                    Some("seedbox-build:private"),
                    true,
                    false,
                )
            });
            let deadline = Instant::now() + Duration::from_secs(30);
            while fs::read_to_string(&ready)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .is_none()
            {
                let within_deadline = Instant::now() < deadline;
                if !within_deadline {
                    token.cancel();
                }
                assert!(within_deadline);
                std::thread::yield_now();
            }
            token.cancel();
            assert!(matches!(
                task.join().unwrap(),
                Err(Error::PreparationCleanupUnverified(_))
            ));
        });
        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.path().join("evidence/build.json")).unwrap(),
        )
        .unwrap();
        assert!(receipt["cleanup"]
            .as_str()
            .unwrap()
            .contains("private tag retained"));
        assert!(!directory.path().join("evidence/tag-cleanup.json").exists());
    }
    #[test]
    fn custom_builder_runs_local_check_and_evidence_failure_prevents_spawn() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("executed");
        let path = script(
            directory.path(),
            "wrapper",
            &format!("echo yes > '{}'", marker.display()),
        );
        let context = PreparationContext::new(
            directory.path().join("evidence"),
            CancellationToken::default(),
        )
        .unwrap();
        assert!(context.admit_builder(&path).is_ok());
        assert!(marker.exists());
        fs::remove_file(&marker).unwrap();
        fs::remove_dir_all(&context.directory).unwrap();
        fs::write(&context.directory, b"not a directory").unwrap();
        assert!(context
            .run(&mut Command::new(path), "probe", None, false, false)
            .is_err());
        assert!(!marker.exists());
    }
    #[test]
    fn failure_bytes_and_cleanup_failure_remain_readable_after_context_drop() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = script(
            directory.path(),
            "fail",
            "printf 'copy failed\\377'; exit 7",
        );
        let context = PreparationContext::new(
            directory.path().join("evidence"),
            CancellationToken::default(),
        )
        .unwrap();
        assert!(matches!(
            context.run(&mut Command::new(&path), "copy", None, false, false),
            Err(Error::Execution(_))
        ));
        context.cancellation.cancel();
        assert!(matches!(
            context.run(
                &mut Command::new(&path),
                "tag-cleanup",
                Some("seedbox-build:private"),
                false,
                true
            ),
            Err(Error::Execution(_))
        ));
        drop(context);
        for phase in ["copy", "tag-cleanup"] {
            assert_eq!(
                fs::read(directory.path().join(format!("evidence/{phase}.log"))).unwrap(),
                b"copy failed\xff"
            );
            let receipt: serde_json::Value = serde_json::from_slice(
                &fs::read(directory.path().join(format!("evidence/{phase}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["reaped"], true);
            assert!(receipt["status"].as_str().unwrap().contains('7'));
        }
    }
    /// Test-only subprocess fixture; never exposed by the production CLI.
    #[test]
    #[ignore = "invoked only by owned process control tests"]
    fn owned_process_fixture() {
        let mode = std::env::var("SEEDBOX_FIXTURE_MODE").unwrap();
        let ready = PathBuf::from(std::env::var_os("SEEDBOX_FIXTURE_READY").unwrap());
        if mode == "ownership-loss" || mode == "evidence-loss" {
            let root = ready.parent().unwrap();
            let context = root.join("context");
            fs::create_dir(&context).unwrap();
            fs::write(context.join("Dockerfile"), "FROM scratch\n").unwrap();
            let release = root.join("release");
            let removed = root.join("tag-removed");
            let transport = script(root, "transport", "exit 0");
            let builder=script(root,"builder",&format!("if [ \"$1\" = --version ]; then echo fixture-builder; exit 0; fi; if [ \"$1\" = --remote=false ]; then shift; fi; if [ \"$1\" = build ]; then echo $$ > '{}'; while [ ! -e '{}' ]; do :; done; exit 0; fi; echo cleanup > '{}'",ready.display(),release.display(),removed.display()));
            let simulation=crate::config::parse("name: ownership-loss\nservices:\n  probe: {build: ./context}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: probe}\n",&root.join("compose.yaml"),&std::collections::BTreeMap::new()).unwrap();
            let tools = crate::oci::OciTools {
                skopeo: transport.clone(),
                umoci: transport,
                docker: builder,
                build_epoch: None,
            };
            let evidence =
                PreparationContext::new(root.join("evidence"), CancellationToken::default())
                    .unwrap();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while fs::read_to_string(&ready)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok())
                        .is_none()
                    {
                        let within_deadline = Instant::now() < deadline;
                        if !within_deadline {
                            evidence.cancellation.cancel();
                        }
                        assert!(within_deadline);
                        std::thread::yield_now();
                    }
                    if mode == "ownership-loss" {
                        unsafe {
                            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
                        };
                    } else {
                        fs::rename(root.join("evidence"), root.join("preserved-evidence")).unwrap();
                        fs::write(
                            root.join("evidence"),
                            b"metadata destination became unavailable",
                        )
                        .unwrap();
                    }
                    fs::write(&release, b"exit now").unwrap();
                });
                let result = tools.import_with_context(
                    &simulation.services["probe"],
                    &root.join("import"),
                    &crate::store::ContentStore::new(root.join("store")),
                    Some((&evidence, "probe")),
                );
                assert!(matches!(
                    result,
                    Err(Error::PreparationCleanupUnverified(_))
                ));
                assert!(!removed.exists());
            });
            return;
        } else if mode == "grandchild" {
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        } else if mode == "delegated" {
            assert!(unsafe { libc::setsid() } >= 0);
            fs::write(&ready, std::process::id().to_string()).unwrap();
        } else {
            let executable = std::env::current_exe().unwrap();
            let mut child = Command::new(executable)
                .args([
                    "--exact",
                    "preparation_command::tests::owned_process_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .env(
                    "SEEDBOX_FIXTURE_MODE",
                    if mode == "client" {
                        "delegated"
                    } else {
                        "grandchild"
                    },
                )
                .spawn()
                .unwrap();
            if mode == "parent" {
                fs::write(&ready, child.id().to_string()).unwrap();
            }
            // This child belongs only to the test. Its owner is intentionally
            // terminated by the command runner; the test checks the resulting PID.
            let _ = child.wait();
            return;
        }
        loop {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn escaped_delegated_work_is_unverified_and_private_tag_is_retained() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let token = CancellationToken::default();
        let context =
            PreparationContext::new(directory.path().join("evidence"), token.clone()).unwrap();
        let executable = std::env::current_exe().unwrap();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| {
                context.run(
                    Command::new(executable)
                        .args([
                            "--exact",
                            "preparation_command::tests::owned_process_fixture",
                            "--ignored",
                            "--nocapture",
                        ])
                        .env("SEEDBOX_FIXTURE_MODE", "client")
                        .env("SEEDBOX_FIXTURE_READY", &ready),
                    "delegated",
                    Some("seedbox-build:owned"),
                    true,
                    false,
                )
            });
            let deadline = Instant::now() + Duration::from_secs(30);
            let pid = loop {
                if let Some(pid) = fs::read_to_string(&ready)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    break pid;
                }
                let within_deadline = Instant::now() < deadline;
                if !within_deadline {
                    token.cancel();
                }
                assert!(within_deadline);
                std::thread::yield_now();
            };
            token.cancel();
            let result = task.join().unwrap();
            assert!(matches!(
                result,
                Err(Error::PreparationCleanupUnverified(_))
            ));
            assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            };
        });
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(context.directory.join("delegated.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["output_complete"], false);
        assert_eq!(receipt["cleanup"], "cleanup-unverified");
        assert_eq!(receipt["private_tag"], "seedbox-build:owned");
    }
    #[test]
    fn cancellation_stops_owned_grandchild_and_reaps_leader() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("grandchild");
        let token = CancellationToken::default();
        let context =
            PreparationContext::new(directory.path().join("evidence"), token.clone()).unwrap();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| {
                context.run(
                    Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "preparation_command::tests::owned_process_fixture",
                            "--ignored",
                            "--nocapture",
                        ])
                        .env("SEEDBOX_FIXTURE_MODE", "parent")
                        .env("SEEDBOX_FIXTURE_READY", &ready),
                    "parent",
                    None,
                    false,
                    false,
                )
            });
            let deadline = Instant::now() + Duration::from_secs(30);
            let pid = loop {
                if let Some(pid) = fs::read_to_string(&ready)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    break pid;
                }
                let within_deadline = Instant::now() < deadline;
                if !within_deadline {
                    token.cancel();
                }
                assert!(within_deadline);
                std::thread::yield_now();
            };
            token.cancel();
            assert!(matches!(
                task.join().unwrap(),
                Err(Error::PreparationCancelled)
            ));
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if unsafe { libc::kill(pid, 0) } == -1 {
                    break;
                }
                #[cfg(target_os = "linux")]
                if fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .is_some_and(|s| {
                        s.split_once(") ")
                            .is_some_and(|(_, tail)| tail.starts_with('Z'))
                    })
                {
                    break;
                }
                assert!(Instant::now() < deadline, "owned grandchild still running");
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(context.directory.join("parent.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["reaped"], true);
        assert_eq!(receipt["output_complete"], true);
    }
    #[test]
    fn actual_lost_child_ownership_keeps_typed_uncertainty_and_blocks_tag_removal() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "preparation_command::tests::owned_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("SEEDBOX_FIXTURE_MODE", "ownership-loss")
            .env("SEEDBOX_FIXTURE_READY", &ready)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.path().join("evidence/probe-build.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["reaped"], false);
        assert!(receipt["cleanup"].as_str().unwrap().contains("unverified"));
        assert!(!directory.path().join("tag-removed").exists());
        assert!(!directory
            .path()
            .join("evidence/probe-tag-cleanup.json")
            .exists());
    }
    #[test]
    fn required_post_build_evidence_failure_blocks_private_tag_cleanup() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "preparation_command::tests::owned_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("SEEDBOX_FIXTURE_MODE", "evidence-loss")
            .env("SEEDBOX_FIXTURE_READY", &ready)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(directory
            .path()
            .join("preserved-evidence/probe-build.log")
            .is_file());
        assert!(!directory.path().join("tag-removed").exists());
        assert!(!directory
            .path()
            .join("preserved-evidence/probe-tag-cleanup.json")
            .exists());
    }
}

#[cfg(all(test, unix))]
mod drain_race_tests {
    use super::*;
    #[test]
    fn delayed_timeout_observation_after_child_exit() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let context =
            PreparationContext::new(root.join("evidence"), CancellationToken::default()).unwrap();
        let result = context.run(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "while [ ! -e \"$1\" ]; do sleep 0.01; done; printf 'copy failed'; exit 7",
                    "fixture",
                ])
                .arg(context.directory.join("release-child")),
            "test-timeout-window",
            None,
            false,
            false,
        );
        let receipt = fs::read_to_string(root.join("evidence/test-timeout-window.json")).unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["reaped"], true);
        assert_eq!(receipt["output_complete"], true);
        assert!(
            matches!(result, Err(Error::Execution(_))),
            "completed child should retain execution failure, not false cleanup uncertainty"
        );
        assert_eq!(
            fs::read(root.join("evidence/test-timeout-window.log")).unwrap(),
            b"copy failed"
        );
    }
    #[test]
    fn delayed_required_stdout_timeout_after_child_exit() {
        let _process_creation = PROCESS_TEST_LOCK.lock().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let context =
            PreparationContext::new(root.join("evidence"), CancellationToken::default()).unwrap();
        let mut bytes = Vec::new();
        let result = context.run_owned(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "while [ ! -e \"$1\" ]; do sleep 0.01; done; printf 'fixture-v1'; exit 0",
                    "fixture",
                ])
                .arg(context.directory.join("release-child")),
            "test-timeout-stdout",
            None,
            false,
            false,
            Some(&mut bytes),
        );
        let receipt: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join("evidence/test-timeout-stdout.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["reaped"], true);
        assert_eq!(receipt["output_complete"], true);
        assert!(result.is_ok());
        assert_eq!(bytes, b"fixture-v1");
    }
}
