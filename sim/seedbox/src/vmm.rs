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
    let faults = compile_faults(request)?;
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
        "machines":machines,"faults":faults,
    }))
}

/// Compile service targets using retained placement, never name coincidence.
fn compile_faults(request: &ExecutionRequest<'_>) -> Result<Vec<Value>> {
    use crate::config::{FaultAction, FaultTarget};
    let mut active = BTreeSet::new();
    let mut used = BTreeSet::new();
    let mut stopped = BTreeSet::new();
    let mut previous = 0;
    let resolve = |target: &FaultTarget| -> Result<String> {
        let machine = match target {
            FaultTarget::Service(service) => request
                .prepared
                .placement
                .get(service)
                .ok_or_else(|| Error::Execution(format!("unknown fault service {service}")))?,
            FaultTarget::Machine(machine) => machine,
        };
        if !request.prepared.guests.contains_key(machine) {
            return Err(Error::Execution(format!("unknown fault machine {machine}")));
        }
        Ok(machine.clone())
    };
    request
        .faults
        .iter()
        .enumerate()
        .map(|(i, fault)| {
            if fault.at_ns < previous || fault.at_ns >= request.limits.simulated_time_ns {
                return Err(Error::Execution(
                    "fault times must be ordered and precede limit".into(),
                ));
            }
            previous = fault.at_ns;
            let mut value = match &fault.action {
                FaultAction::Stop { target } | FaultAction::Restart { target } => {
                    let machine = resolve(target)?;
                    let stopping = matches!(fault.action, FaultAction::Stop { .. });
                    if (stopping && !stopped.insert(machine.clone()))
                        || (!stopping && !stopped.remove(&machine))
                    {
                        return Err(Error::Execution(
                            "invalid stop/restart state after placement".into(),
                        ));
                    }
                    json!({"action":if stopping {"stop"} else {"restart"}, "machine":machine})
                }
                FaultAction::Partition { id, left, right } => {
                    if id.is_empty() || !used.insert(id.clone()) {
                        return Err(Error::Execution("duplicate/empty partition id".into()));
                    }
                    let left = left.iter().map(&resolve).collect::<Result<Vec<_>>>()?;
                    let right = right.iter().map(&resolve).collect::<Result<Vec<_>>>()?;
                    let unique: BTreeSet<_> = left.iter().chain(&right).collect();
                    if left.is_empty()
                        || right.is_empty()
                        || unique.len() != left.len() + right.len()
                    {
                        return Err(Error::Execution(
                            "partition sides overlap after placement".into(),
                        ));
                    }
                    active.insert(id.clone());
                    json!({"action":"partition","handle":id,"left":left,"right":right})
                }
                FaultAction::Heal { id } => {
                    if !active.remove(id) {
                        return Err(Error::Execution("heal of inactive partition".into()));
                    }
                    json!({"action":"heal","handle":id})
                }
            };
            value["id"] = json!(
                u32::try_from(i + 1).map_err(|_| Error::Execution("too many faults".into()))?
            );
            value["at_ns"] = json!(fault.at_ns);
            value["deadline_ns"] = json!(fault
                .at_ns
                .saturating_add(5_000_000_000)
                .min(request.limits.simulated_time_ns));
            Ok(value)
        })
        .collect()
}
#[derive(Deserialize)]
struct RawFault {
    request: Value,
    id: u32,
    requested_at_ns: u64,
    applied_at_ns: Option<u64>,
    action: String,
    machine: Option<String>,
    status: String,
    detail: String,
}

#[derive(Deserialize)]
struct MachineReport {
    name: String,
    infrastructure_error: bool,
    guest_exit_code: Option<u8>,
    guest_init_error: Option<u8>,
    state_digest: Option<ContentDigest>,
}
#[derive(Deserialize)]
struct VmmReport {
    report_version: u32,
    seed: u64,
    outcome: String,
    world_witness: ContentDigest,
    machines: Vec<MachineReport>,
    #[serde(default)]
    faults: Vec<RawFault>,
    #[serde(default)]
    fault_error: Option<String>,
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
    let mut reason = if infrastructure_error.is_some() {
        StopReason::Error
    } else {
        match raw.outcome.as_str() {
            "end_machine_exited" => StopReason::Completed,
            "duration" => StopReason::SimulatedTimeLimit,
            "quiescent" => StopReason::Quiescent,
            "fault_failed" => StopReason::Error,
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
    let expected = compile_faults(request)?;
    let mut faults = Vec::new();
    let mut fault_failure = raw.fault_error;
    if raw.faults.len() != expected.len() {
        fault_failure = Some("fault request history is incomplete".to_string());
    }
    for (i, fault) in raw.faults.iter().enumerate() {
        let Some(plan) = expected.get(i) else {
            return Err(Error::Integrity("unexpected fault acknowledgement".into()));
        };
        if fault.request != *plan
            || fault.id as usize != i + 1
            || Some(fault.requested_at_ns) != plan["at_ns"].as_u64()
            || Some(fault.action.as_str()) != plan["action"].as_str()
            || fault.machine.as_deref() != plan["machine"].as_str()
        {
            return Err(Error::Integrity(
                "fault acknowledgement correlation mismatch".into(),
            ));
        }
        match (fault.status.as_str(), fault.applied_at_ns) {
            ("applied", Some(at))
                if at >= fault.requested_at_ns && at <= plan["deadline_ns"].as_u64().unwrap() =>
            {
                let service = fault
                    .machine
                    .as_ref()
                    .and_then(|machine| {
                        request
                            .prepared
                            .placement
                            .iter()
                            .find(|(_, m)| *m == machine)
                            .map(|(s, _)| s.clone())
                    })
                    .unwrap_or_else(|| request.check.service.clone());
                faults.push(crate::execution::FaultEvent {
                    requested_at_ns: fault.requested_at_ns,
                    applied_at_ns: at,
                    service,
                    action: fault.action.clone(),
                    detail: format!("id={} {}", fault.id, fault.detail),
                });
            }
            ("requested" | "failed" | "unrequested", None) => {
                fault_failure = Some(format!(
                    "fault {} {}: {}",
                    fault.id, fault.status, fault.detail
                ))
            }
            _ => return Err(Error::Integrity("invalid fault effect status/time".into())),
        }
    }
    if fault_failure.is_some() {
        reason = StopReason::Error;
    }
    Ok(ExecutionReport {
        reason,
        workload_exit,
        witness: Some(raw.world_witness),
        full_state,
        faults,
        detail: fault_failure.or_else(|| {
            infrastructure_error
                .map(|machine| format!("machine {} failed in guest bootstrap or VMM", machine.name))
        }),
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
                    report(&fs::read(&report_path)?, &request)
                } else {
                    Err(Error::Execution("VMM failed; see vmm.stderr".into()))
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
