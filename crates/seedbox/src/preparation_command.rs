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

const FILE_LIMIT: u64 = 4 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 64 * 1024 * 1024;
const METADATA_LIMIT: u64 = 4 * 1024 * 1024;

/// Caller-owned preparation evidence and the shared cancellation token.
#[derive(Debug)]
pub struct PreparationContext {
    pub cancellation: CancellationToken,
    pub directory: PathBuf,
    quotas: Mutex<(u64, u64)>,
    admitted_builder: Mutex<Option<PathBuf>>,
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
    escalated: bool,
    reaped: bool,
    cleanup: String,
    elapsed_seconds: f64,
    retained_bytes: u64,
    omitted_bytes: u64,
    omission_reason: Option<String>,
    output_complete: bool,
    pid: Option<u32>,
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
            quotas: Mutex::new((0, 0)),
            admitted_builder: Mutex::new(None),
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
        let bytes = serde_json::to_vec_pretty(value)?;
        let mut quota = self
            .quotas
            .lock()
            .map_err(|_| Error::Execution("preparation evidence lock poisoned".into()))?;
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
    }
    /// Admit only the inspected immutable project-Nix launcher/native closure.
    pub(crate) fn admit_builder(&self, requested: &Path) -> Result<()> {
        let launcher = fs::canonicalize(requested).map_err(|e| {
            Error::Setup(format!(
                "builder {}: {e}; enter the qualified project Nix shell",
                requested.display()
            ))
        })?;
        let native = launcher.with_file_name(".podman-wrapped");
        let launch_hash = ContentDigest::of(&fs::read(&launcher)?);
        let native_hash = fs::read(&native).map(|b| ContentDigest::of(&b)).ok();
        let admitted = cfg!(target_os = "linux")
            && launcher.starts_with("/nix/store")
            && launch_hash.hex()
                == "e08e27ef8cbb231dd85f8f343d7af0428c7b996ace2f17ee78bd6c4391a3aa06"
            && native_hash.as_ref().is_some_and(|h| {
                h.hex() == "94a99b74250736f3f89426be43870d499aa6a03a53c02fe9ca6753e324255344"
            });
        self.metadata("builder.json", &serde_json::json!({"requested": requested, "launcher": launcher, "launcher_sha256": launch_hash, "native": native, "native_sha256": native_hash, "local_mode": true, "admitted": admitted, "cancellation_cleanup_qualified": false}))?;
        if !admitted {
            return Err(Error::Setup(format!("builder {}: exploration requires the inspected project-Nix local Podman launch closure; unknown wrappers, remote clients and daemon delegation are unsupported", requested.display())));
        }
        *self
            .admitted_builder
            .lock()
            .map_err(|_| Error::Execution("builder admission lock poisoned".into()))? =
            Some(launcher);
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
        self.run_owned(command, phase, tag, build, cleanup)
    }
    #[cfg(unix)]
    fn run_owned(
        &self,
        command: &mut Command,
        phase: &str,
        tag: Option<&str>,
        build: bool,
        cleanup: bool,
    ) -> Result<()> {
        use std::os::unix::{net::UnixStream, process::CommandExt};
        let program = PathBuf::from(command.get_program());
        let resolved = fs::canonicalize(&program)
            .map_err(|e| crate::oci::spawn_error(program.as_os_str(), e))?;
        if tag.is_some() {
            if let Some(admitted) = self
                .admitted_builder
                .lock()
                .map_err(|_| Error::Execution("builder admission lock poisoned".into()))?
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
            escalated: false,
            reaped: false,
            cleanup: "unfinished".into(),
            elapsed_seconds: 0.0,
            retained_bytes: 0,
            omitted_bytes: 0,
            omission_reason: None,
            output_complete: false,
            pid: None,
        };
        let metadata = format!("{phase}.json");
        self.metadata(&metadata, &receipt)?;
        let quota = self
            .quotas
            .lock()
            .map_err(|_| Error::Execution("preparation evidence lock poisoned".into()))?;
        let phase_limit = if phase.ends_with("version") {
            8192
        } else {
            FILE_LIMIT
        };
        let remaining = phase_limit.min(TOTAL_LIMIT.saturating_sub(quota.0));
        drop(quota);
        let log = self.directory.join(format!("{phase}.log"));
        let mut file = fs::File::create(&log)?;
        let (mut reader, writer) = UnixStream::pair()?;
        reader.set_read_timeout(Some(Duration::from_millis(50)))?;
        command
            .process_group(0)
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer.try_clone()?)))
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
                        if done.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            file.sync_all()?;
            Ok((retained, seen - retained, complete))
        });
        let pid = child.id() as i32;
        let mut cancellation_at = None;
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
            if !cleanup && self.cancellation.is_cancelled() && cancellation_at.is_none() {
                receipt.cancelled = true;
                cancellation_at = Some(Instant::now());
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                };
            }
            if !receipt.escalated
                && (cancellation_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(250))
                    || (cleanup && began.elapsed() >= Duration::from_secs(5)))
            {
                receipt.escalated = true;
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                };
            }
            if cancellation_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(5))
                || (cleanup && began.elapsed() >= Duration::from_millis(5250))
            {
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
        let output = drain
            .join()
            .map_err(|_| Error::Execution("preparation output reader panicked".into()))?;
        if let Ok((retained, omitted, complete)) = &output {
            receipt.output_complete = *complete;
            receipt.retained_bytes = *retained;
            receipt.omitted_bytes = *omitted;
            self.quotas
                .lock()
                .map_err(|_| Error::Execution("preparation evidence lock poisoned".into()))?
                .0 += *retained;
            if *omitted > 0 {
                receipt.omission_reason = Some("phase or total payload quota".into());
            }
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
        output?;
        if !receipt.output_complete {
            return Err(Error::PreparationCleanupUnverified(format!(
                "{phase}: output stream remained open after cleanup; private tag retained"
            )));
        }
        let status = status.map_err(|e| {
            Error::Execution(format!(
                "command {}: {e}; cleanup-unverified",
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
    ) -> Result<()> {
        Err(Error::Setup(
            "exploration preparation requires Unix owned process groups".into(),
        ))
    }
    pub(crate) fn version(&self, tool: &Path, phase: &str) -> Result<String> {
        self.run(
            Command::new(tool).arg("--version"),
            phase,
            None,
            false,
            false,
        )?;
        let mut bytes = Vec::new();
        fs::File::open(self.directory.join(format!("{phase}.log")))?
            .take(8192)
            .read_to_end(&mut bytes)?;
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
            let deadline = Instant::now() + Duration::from_secs(3);
            while fs::read_to_string(&ready)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .is_none()
            {
                assert!(Instant::now() < deadline);
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
            let deadline = Instant::now() + Duration::from_secs(3);
            while fs::read_to_string(&ready)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .is_none()
            {
                assert!(Instant::now() < deadline);
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
    fn unknown_builder_rejected_before_execution_and_evidence_failure_prevents_spawn() {
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
        assert!(context.admit_builder(&path).is_err());
        assert!(!marker.exists());
        fs::remove_file(context.directory.join("builder.json")).unwrap();
        fs::remove_dir(&context.directory).unwrap();
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
        if mode == "grandchild" {
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
            let deadline = Instant::now() + Duration::from_secs(3);
            let pid = loop {
                if let Some(pid) = fs::read_to_string(&ready)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    break pid;
                }
                assert!(Instant::now() < deadline);
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
            let deadline = Instant::now() + Duration::from_secs(3);
            let pid = loop {
                if let Some(pid) = fs::read_to_string(&ready)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    break pid;
                }
                assert!(Instant::now() < deadline);
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
}
