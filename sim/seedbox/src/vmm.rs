//! Adapter for the controlled Seedbox KVM engine. World JSON is an internal
//! execution artifact generated from verified inputs, not the authoring format.

use crate::{
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Debug)]
pub struct VmmExecutor {
    pub cpu: u32,
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
}

impl VmmExecutor {
    pub fn new(
        engine: &Path,
        store: &ContentStore,
        cpu: u32,
        memory_mb: u32,
        witness_mode: WitnessMode,
    ) -> Result<Self> {
        if !(64..=65536).contains(&memory_mb) {
            return Err(Error::Execution(
                "guest memory must be 64..65536 MiB".into(),
            ));
        }
        Ok(Self {
            cpu,
            profile: ExecutionProfile {
                backend: "seedbox-kvm".into(),
                engine: store.put_file(engine)?,
                witness_mode,
                semantic_parameters: BTreeMap::from([
                    ("guest_abi".into(), "oci-init-v1".into()),
                    ("memory_mb".into(), memory_mb.to_string()),
                ]),
            },
            performance: BTreeMap::from([
                ("SEEDBOX_MARGIN".into(), "adaptive".into()),
                ("SEEDBOX_UNDO".into(), "256".into()),
            ]),
        })
    }

    pub fn from_profile(profile: ExecutionProfile, cpu: u32) -> Result<Self> {
        let memory = profile
            .semantic_parameters
            .get("memory_mb")
            .and_then(|s| s.parse::<u32>().ok());
        if profile.backend != "seedbox-kvm"
            || !memory.is_some_and(|m| (64..=65536).contains(&m))
            || !matches!(
                profile
                    .semantic_parameters
                    .get("guest_abi")
                    .map(String::as_str),
                Some("oci-init-v1" | crate::fixture::ABI)
            )
        {
            return Err(Error::Integrity("invalid KVM profile".into()));
        }
        Ok(Self {
            cpu,
            profile,
            performance: BTreeMap::from([
                ("SEEDBOX_MARGIN".into(), "adaptive".into()),
                ("SEEDBOX_UNDO".into(), "256".into()),
            ]),
        })
    }
    pub fn with_fixture(mut self) -> Self {
        self.profile
            .semantic_parameters
            .insert("guest_abi".into(), crate::fixture::ABI.into());
        self
    }
    pub fn with_margin(mut self, margin: &str) -> Result<Self> {
        if margin != "adaptive" && margin.parse::<u64>().ok().is_none_or(|value| value == 0) {
            return Err(Error::Execution(
                "margin must be adaptive or a positive integer".into(),
            ));
        }
        self.performance
            .insert("SEEDBOX_MARGIN".into(), margin.into());
        Ok(self)
    }

    /// Initial undo margin in branches; zero disables speculative stop recovery.
    pub fn with_undo(mut self, branches: u64) -> Result<Self> {
        if branches > (1 << 18) {
            return Err(Error::Execution("undo margin must be 0..262144".into()));
        }
        self.performance
            .insert("SEEDBOX_UNDO".into(), branches.to_string());
        Ok(self)
    }
}

impl ExecutionBackend for VmmExecutor {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.performance.clone()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        #[cfg(target_os = "linux")]
        {
            linux::execute(self, request)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            Err(Error::Execution(
                "KVM execution requires the qualified Linux amd64 host".into(),
            ))
        }
    }
}

/// Translate an execution request without consulting registry tags or source
/// paths. Exposed for preparation tools and report inspection without KVM.
pub fn world(request: &ExecutionRequest<'_>) -> Result<Value> {
    if request
        .prepared
        .profile
        .semantic_parameters
        .get("guest_abi")
        .map(String::as_str)
        == Some(crate::fixture::ABI)
    {
        return crate::fixture::world(request);
    }
    if request.prepared.guests.is_empty() || request.prepared.guests.len() > 254 {
        return Err(Error::Integrity("expected 1..254 prepared machines".into()));
    }
    let memory: u32 = request
        .prepared
        .profile
        .semantic_parameters
        .get("memory_mb")
        .ok_or_else(|| Error::Integrity("profile missing memory_mb".into()))?
        .parse()
        .map_err(|_| Error::Integrity("invalid memory_mb".into()))?;
    let artifact = |digest: &ContentDigest| {
        let path = request.artifacts.get(digest).ok_or_else(|| {
            Error::Integrity(format!("missing verified artifact {}", digest.hex()))
        })?;
        Ok::<_, Error>(std::path::absolute(path)?)
    };
    let workload_machine = request
        .prepared
        .placement
        .get(&request.check.service)
        .ok_or_else(|| Error::Integrity("missing workload placement".into()))?;
    // This guest ABI launches one application in each machine. The public
    // execution contract also permits other backends to co-locate services.
    if request.prepared.placement.len() != request.prepared.guests.len()
        || request.prepared.placement.values().collect::<BTreeSet<_>>()
            != request.prepared.guests.keys().collect::<BTreeSet<_>>()
    {
        return Err(Error::Execution(
            "oci-init-v1 requires one service per machine".into(),
        ));
    }
    let machines = request
        .prepared
        .guests
        .iter()
        .map(|(name, digest)| Ok(json!({"name":name,"initrd":artifact(digest)?})))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "kernel":artifact(&request.prepared.kernel)?, "mem_mb":memory,
        "duration_ns":request.limits.simulated_time_ns,"end_on_exit":workload_machine,
        "timer_jitter_ns":request.schedule.timer_jitter_ns,"stop_tolerance_ns":request.schedule.stop_tolerance_ns,
        "network":{"latency_ns":request.network.latency_ns,"jitter_ns":request.network.jitter_ns},
        "machines":machines,
    }))
}

#[derive(Deserialize)]
struct MachineReport {
    witness: Option<ContentDigest>,
    exits: Option<u64>,
    name: String,
    infrastructure_error: bool,
    guest_exit_code: Option<u8>,
    guest_init_error: Option<u8>,
    state_digest: Option<ContentDigest>,
}
#[derive(Deserialize)]
struct VmmReport {
    sim_seconds: Option<f64>,
    host_seconds: Option<f64>,
    #[serde(default)]
    kills: Vec<ScheduledKill>,
    report_version: u32,
    seed: u64,
    outcome: String,
    world_witness: ContentDigest,
    machines: Vec<MachineReport>,
}

#[derive(Deserialize)]
struct ScheduledKill {
    member: String,
    stop_ns: u64,
    start_ns: u64,
}

/// Parse the engine's structured completion report. Never infer application
/// exit status from log text or the VMM's infrastructure sentinel exit codes.
pub fn report(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let raw: VmmReport = serde_json::from_slice(bytes)?;
    if raw.report_version != 1 || raw.seed != request.seed {
        return Err(Error::Integrity("VMM report version/seed mismatch".into()));
    }
    let names: BTreeSet<_> = raw.machines.iter().map(|machine| &machine.name).collect();
    if names.len() != raw.machines.len() || names != request.prepared.guests.keys().collect() {
        return Err(Error::Integrity("VMM report machine set mismatch".into()));
    }
    let workload = request
        .prepared
        .placement
        .get(&request.check.service)
        .ok_or_else(|| Error::Integrity("missing workload placement".into()))?;
    let workload_exit = raw
        .machines
        .iter()
        .find(|machine| &machine.name == workload)
        .and_then(|machine| machine.guest_exit_code)
        .map(|code| WorkloadExit {
            service: request.check.service.clone(),
            code,
        });
    let infrastructure_error = raw
        .machines
        .iter()
        .find(|machine| machine.guest_init_error.is_some() || machine.infrastructure_error);
    let reason = if infrastructure_error.is_some() {
        StopReason::Error
    } else {
        match raw.outcome.as_str() {
            "end_machine_exited" => StopReason::Completed,
            "duration" => StopReason::SimulatedTimeLimit,
            "quiescent" => StopReason::Quiescent,
            other => return Err(Error::Integrity(format!("unknown VMM outcome {other}"))),
        }
    };
    let full_state = match request.prepared.profile.witness_mode {
        WitnessMode::Exits => None,
        WitnessMode::FullState => Some(
            raw.machines
                .iter()
                .map(|machine| {
                    Ok((
                        machine.name.clone(),
                        machine.state_digest.clone().ok_or_else(|| {
                            Error::Integrity("missing machine state digest".into())
                        })?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        ),
    };
    let diagnostics = if raw
        .machines
        .iter()
        .all(|m| m.witness.is_some() && m.exits.is_some())
    {
        let ns = |seconds: Option<f64>| -> Result<u64> {
            let seconds =
                seconds.ok_or_else(|| Error::Integrity("missing execution timing".into()))?;
            if !seconds.is_finite() || seconds < 0.0 || seconds * 1e9 >= u64::MAX as f64 {
                return Err(Error::Integrity("invalid execution timing".into()));
            }
            Ok((seconds * 1e9).round() as u64)
        };
        Some(ExecutionDiagnostics {
            machine_witnesses: raw
                .machines
                .iter()
                .map(|m| (m.name.clone(), m.witness.clone().unwrap()))
                .collect(),
            exits: raw
                .machines
                .iter()
                .map(|m| (m.name.clone(), m.exits.unwrap()))
                .collect(),
            simulated_time_ns: ns(raw.sim_seconds)?,
            host_time_ns: ns(raw.host_seconds)?,
            legacy_faults: raw
                .kills
                .into_iter()
                .map(|k| LegacyFaultObservation {
                    member: k.member,
                    stop_ns: k.stop_ns,
                    start_ns: k.start_ns,
                    stopped: false,
                    restarted: false,
                })
                .collect(),
            checker_result: None,
        })
    } else {
        None
    };
    Ok(ExecutionReport {
        reason,
        workload_exit,
        witness: Some(raw.world_witness),
        full_state,
        faults: vec![],
        diagnostics,
        detail: infrastructure_error
            .map(|machine| format!("machine {} failed in guest bootstrap or VMM", machine.name)),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::host::HostLease;
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        process::{Child, Command},
        time::{Duration, Instant},
    };

    // VMM is a single-process engine. Keep its unreaped leader owned until the
    // final process-group signal, including when it has already exited.
    struct OwnedChild(Option<Child>);
    impl OwnedChild {
        fn stop(&mut self) -> Result<()> {
            if let Some(child) = &mut self.0 {
                let killed = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                if killed != 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                child.wait()?;
                self.0 = None;
            }
            Ok(())
        }
        fn exited(&self) -> Result<Option<bool>> {
            let child = self.0.as_ref().unwrap();
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
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(None);
                }
                return Err(error.into());
            }
            Ok(unsafe {
                (info.si_pid() != 0)
                    .then(|| info.si_code == libc::CLD_EXITED && info.si_status() == 0)
            })
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            // An owned live child must never outlive its host-state guard.
            // Retry interrupted waits before the guard can restore MSRs.
            while self.0.is_some() {
                if let Err(error) = self.stop() {
                    eprintln!("seedbox: waiting for VMM cleanup: {error}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    pub(super) fn execute(
        backend: &VmmExecutor,
        request: ExecutionRequest<'_>,
    ) -> Result<ExecutionReport> {
        if request.prepared.profile != backend.profile {
            return Err(Error::Integrity("execution profile mismatch".into()));
        }
        if backend.cpu >= libc::CPU_SETSIZE as u32 {
            return Err(Error::Execution("CPU exceeds affinity mask".into()));
        }
        let world = world(&request)?;
        let world_path = request.output.join("world.json");
        fs::write(&world_path, serde_json::to_vec_pretty(&world)?)?;
        let engine = request
            .artifacts
            .get(&backend.profile.engine)
            .ok_or_else(|| Error::Integrity("missing retained VMM".into()))?;
        fs::set_permissions(engine, fs::Permissions::from_mode(0o500))?;
        let report_path = request.output.join("vmm.json");
        let mut command = Command::new(engine);
        command
            .arg("--config")
            .arg(&world_path)
            .arg("--seed")
            .arg(request.seed.to_string())
            .arg("--out")
            .arg(request.output.join("machines"));
        command
            .arg("--stream-console")
            .arg("--console-limit")
            .arg((1024 * 1024 * 1024 / (2 * request.prepared.guests.len())).to_string());
        if backend.profile.witness_mode == WitnessMode::FullState {
            command.arg("--digest");
        }
        // Do not inherit hidden performance/semantic VMM environment settings.
        command.env_clear().envs(request.performance_parameters);
        command
            .stdout(fs::File::create(&report_path)?)
            .stderr(fs::File::create(request.output.join("vmm.stderr"))?);
        let cpu = backend.cpu;
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut set: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_ZERO(&mut set);
                libc::CPU_SET(cpu as usize, &mut set);
                if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let incomplete = |reason| ExecutionReport {
            reason,
            workload_exit: None,
            witness: None,
            full_state: None,
            faults: vec![],
            diagnostics: None,
            detail: None,
        };
        if request.cancellation.is_cancelled() {
            return Ok(incomplete(StopReason::Cancelled));
        }
        let mut lease = HostLease::acquire(cpu, request.output)?;
        let started = Instant::now();
        let mut child = OwnedChild(Some(command.spawn()?));
        let completion = loop {
            if request.cancellation.is_cancelled() {
                break Ok(incomplete(StopReason::Cancelled));
            }
            if let Some(success) = child.exited()? {
                break if success {
                    report(&fs::read(&report_path)?, &request).and_then(|mut report| {
                        if backend
                            .profile
                            .semantic_parameters
                            .get("guest_abi")
                            .map(String::as_str)
                            == Some(crate::fixture::ABI)
                        {
                            crate::fixture::observe(&mut report, &request)?;
                        }
                        Ok(report)
                    })
                } else {
                    {
                        let stderr =
                            crate::fixture::read_console(&request.output.join("vmm.stderr"))?;
                        if stderr.contains("overshoot") && !stderr.contains("clock bound exceeded")
                        {
                            Err(Error::Overshoot(stderr))
                        } else {
                            Err(Error::Execution(format!("VMM failed: {stderr}")))
                        }
                    }
                };
            }
            if started.elapsed() >= Duration::from_nanos(request.limits.wall_time_ns) {
                break Ok(incomplete(StopReason::WallTimeLimit));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        child.stop()?;
        lease.restore()?;
        completion
    }
}
