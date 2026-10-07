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
    /// Host CPU to pin the VMM to; `None` leases the first free eligible core.
    pub cpu: Option<u32>,
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
    admission: std::sync::Arc<std::sync::Mutex<Vec<Vec<crate::admission::CoreObservation>>>>,
}

impl VmmExecutor {
    pub fn new(
        engine: &Path,
        store: &ContentStore,
        cpu: Option<u32>,
        memory_mb: u32,
        witness_mode: WitnessMode,
    ) -> Result<Self> {
        if !(64..=65536).contains(&memory_mb) {
            return Err(Error::Execution(
                "guest memory must be 64..65536 MiB".into(),
            ));
        }
        let mut semantic_parameters = BTreeMap::from([
            ("guest_abi".into(), "oci-init-v1".into()),
            ("control_channel".into(), "virtio-console-v1".into()),
            ("memory_mb".into(), memory_mb.to_string()),
        ]);
        semantic_parameters.extend(crate::admission::bindings()?);
        Ok(Self {
            cpu,
            admission: Default::default(),
            profile: ExecutionProfile {
                backend: "seedbox-kvm".into(),
                engine: store.put_file(engine)?,
                witness_mode,
                semantic_parameters,
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
    fn preflight(&self, output: Option<&Path>) -> Result<()> {
        crate::admission::validate_profile(&self.profile)?;
        let mut observations = Vec::new();
        let collection = (|| -> Result<()> {
            #[cfg(target_os = "linux")]
            {
                let reservation = crate::host::CpuReservation::acquire(self.cpu)?;
                for cpu in &reservation.cpus {
                    observations.push(crate::admission::observe(*cpu)?);
                }
                drop(reservation); // No reservation/perf/affinity spans OCI or diagnostic publication.
            }
            #[cfg(not(target_os = "linux"))]
            {
                observations.push(crate::admission::observe(0)?);
            }
            Ok(())
        })();
        let mut saved = self
            .admission
            .lock()
            .map_err(|_| Error::Execution("host admission evidence lock poisoned".into()))?;
        if saved.len() == 4 {
            saved.remove(0);
        }
        saved.push(observations.clone());
        if let Some(output) = output {
            std::fs::write(
                output.join("host-static.json"),
                serde_json::to_vec_pretty(&*saved)?,
            )?;
            if let Err(error) = &collection {
                std::fs::write(output.join("host-static-error.txt"), error.to_string())?;
            }
        }
        drop(saved);
        collection?;
        for observation in &observations {
            crate::admission::validate_observation(observation)?;
        }
        Ok(())
    }
    fn validate_retained(&self, inputs: &PreparedInputs) -> Result<()> {
        crate::admission::validate_profile(&inputs.profile)?;
        crate::admission::validate_kernel(&inputs.kernel)
    }

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

fn control_latency(request: &ExecutionRequest<'_>) -> Result<u64> {
    request
        .network
        .latency_ns
        .checked_mul(2)
        .and_then(|n| n.checked_add(request.schedule.stop_tolerance_ns.checked_mul(2)?))
        .and_then(|n| n.checked_add(1_000_000))
        .ok_or_else(|| Error::Execution("control latency overflow".into()))
}

/// Translate an execution request without consulting registry tags or source
/// paths. Exposed for preparation tools and report inspection without KVM.
pub fn world(request: &ExecutionRequest<'_>) -> Result<Value> {
    if request
        .control
        .is_some_and(|c| c.service != request.check.service)
    {
        return Err(Error::Execution(
            "controller must also be the check service".into(),
        ));
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
    let control = request.control.map(|c| -> Result<Value> {
        let machine = request.prepared.placement.get(&c.service).ok_or_else(|| Error::Execution("controller placement missing".into()))?;
        let latency_ns = control_latency(request)?;
        Ok(json!({"machine":machine,"latency_ns":latency_ns,"placement":request.prepared.placement}))
    }).transpose()?;
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
        "machines":machines,"control":control,
        "control_channel":"virtio-console-v1",
    }))
}

#[derive(Deserialize)]
struct RawFault {
    request: seedbox_control::Plan,
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
    /// Final machine state, `Exited(code)` once the VM has exited.
    #[serde(default)]
    state: Option<String>,
    /// Final simulated time; an exited VM's clock stops at its exit.
    #[serde(default)]
    time_ns: Option<u64>,
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
    #[serde(default)]
    control_history: Vec<Value>,
    #[serde(default)]
    control_latency_ns: Option<u64>,
}

#[derive(Default)]
struct ValidatedControlHistory {
    plans: Vec<seedbox_control::Plan>,
    effects: BTreeMap<u32, u64>,
    /// Process faults whose target VM exited after admission, before an effect,
    /// with the exit time the call resolved at.
    unavailable: BTreeMap<u32, u64>,
}

/// Correlate guest-originated requests to responses and applied engine actions.
fn validate_control_history(
    history: &[Value],
    latency: Option<u64>,
    request: &ExecutionRequest<'_>,
    failed: bool,
) -> Result<ValidatedControlHistory> {
    let expected_latency = control_latency(request)?;
    let latency = latency
        .filter(|n| *n == expected_latency)
        .ok_or_else(|| Error::Integrity("control latency mismatch".into()))?;
    use seedbox_control::{Action, Operation, Plan, Request, Target, PROCESS_EFFECT_TIMEOUT_NS};
    let controller = request
        .control
        .and_then(|control| request.prepared.placement.get(&control.service));
    let resolve = |target: &Target| -> Result<String> {
        let machine = match target {
            Target::Service(service) => request.prepared.placement.get(service).map(String::as_str),
            Target::Machine(machine) => Some(machine.as_str()),
        }
        .ok_or_else(|| Error::Integrity("invalid control target".into()))?;
        if !request.prepared.guests.contains_key(machine)
            || controller.is_some_and(|name| name == machine)
        {
            return Err(Error::Integrity(
                "invalid fault machine/controller target".into(),
            ));
        }
        Ok(machine.to_owned())
    };
    let bad = || Error::Integrity("control request/response/time correlation mismatch".into());
    let mut expected = ValidatedControlHistory::default();
    let mut previous = 0;
    let mut session = 0;
    let mut public_id = 0u64;
    let mut accepted = 0u64;
    for (i, row) in history.iter().enumerate() {
        if row["status"] == "rejected" && failed {
            continue;
        }
        accepted += 1;
        let r = &row["request"];
        let current_session = match row.get("session_id") {
            Some(value) => value.as_u64().ok_or_else(bad)?,
            None => 1,
        };
        if current_session == 0 || current_session > u32::MAX as u64 || current_session < session {
            return Err(bad());
        }
        public_id = if current_session == session {
            public_id + 1
        } else {
            1
        };
        session = current_session;
        let cutoff = row["status"] == "cut_off";
        let response = row["response"].is_object();
        let metadata = if response { &row["response"] } else { row };
        let t = metadata["requested_at_ns"].as_u64().ok_or_else(bad)?;
        let a = metadata["admitted_at_ns"].as_u64().ok_or_else(bad)?;
        let cut_at = if cutoff {
            Some(row["cut_off_at_ns"].as_u64().ok_or_else(bad)?)
        } else {
            None
        };
        if row["relay_id"].as_u64() != Some(accepted)
            || r["version"] != 1
            || r["id"].as_u64() != Some(public_id)
            || public_id > u32::MAX as u64
            || accepted > u32::MAX as u64
            || t < previous
            || t.checked_add(latency) != Some(a)
            || cut_at.is_some_and(|c| {
                c < t || c > request.limits.simulated_time_ns || i + 1 != history.len()
            })
        {
            return Err(bad());
        }
        let (e, reply) = (
            metadata["effect_at_ns"].as_u64(),
            metadata["replied_at_ns"].as_u64(),
        );
        if e.is_some_and(|e| e < a)
            || reply.is_some_and(|r| e.and_then(|e| e.checked_add(latency)) != Some(r))
        {
            return Err(bad());
        }
        if response {
            let e = e.ok_or_else(bad)?;
            let reply = reply.ok_or_else(bad)?;
            if metadata["version"] != 1
                || metadata["id"] != r["id"]
                || e.checked_add(latency) != Some(reply)
                || (!cutoff && reply >= request.limits.simulated_time_ns)
                || if cut_at.is_some() {
                    !row["delivered_at_ns"].is_null()
                } else {
                    row["delivered_at_ns"].as_u64() != Some(reply) && !failed
                }
            {
                return Err(bad());
            }
            previous = reply;
            if metadata["ok"] != true {
                if metadata["ok"] != false || !metadata["error"].is_object() {
                    return Err(Error::Integrity("invalid control error reply".into()));
                }
                if row["fault_id"].is_null() || metadata["error"]["code"] != "target_unavailable" {
                    continue;
                }
                if row["fault_id"].as_u64() != Some(expected.plans.len() as u64 + 1) {
                    return Err(bad());
                }
            }
        } else {
            let c = cut_at.ok_or_else(bad)?;
            let admitted = row["admitted_done"].as_bool().ok_or_else(bad)?;
            if (admitted && a > c) || (!admitted && a < c) {
                return Err(bad());
            }
            if row["fault_id"].is_null() {
                if !row["plan"].is_null() && admitted {
                    return Err(bad());
                }
                continue;
            }
            if !admitted || row["fault_id"].as_u64() != Some(expected.plans.len() as u64 + 1) {
                return Err(bad());
            }
        }
        let unavailable = if response {
            metadata["ok"] == false
        } else {
            e.is_some() && row["error"].is_string() && row["fatal"] == false
        };
        let call: Request = serde_json::from_value(r.clone()).map_err(|_| bad())?;
        let action = match call.operation {
            Operation::Stop { target } => Action::Stop {
                machine: resolve(&target)?,
            },
            Operation::Restart { target } => Action::Restart {
                machine: resolve(&target)?,
            },
            Operation::Partition { left, right } => {
                let handle = if response {
                    &metadata["result"]["handle"]
                } else {
                    &row["plan"]["handle"]
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty() || (response && e != Some(a)) {
                    return Err(bad());
                }
                Action::Partition {
                    handle,
                    left: left.iter().map(&resolve).collect::<Result<_>>()?,
                    right: right.iter().map(&resolve).collect::<Result<_>>()?,
                }
            }
            Operation::Heal { handle } => {
                if response && e != Some(a) {
                    return Err(bad());
                }
                Action::Heal { handle }
            }
            Operation::Now {} => {
                if e != Some(a) || metadata["result"]["now_ns"].as_u64() != e {
                    return Err(bad());
                }
                continue;
            }
            Operation::Inspect {} => {
                if e != Some(a) {
                    return Err(bad());
                }
                continue;
            }
            Operation::Sleep { duration_ns } => {
                if a.checked_add(duration_ns) != e {
                    return Err(bad());
                }
                continue;
            }
        };
        let plan = Plan {
            id: u32::try_from(expected.plans.len() + 1).map_err(|_| bad())?,
            at_ns: a,
            deadline_ns: a.checked_add(PROCESS_EFFECT_TIMEOUT_NS).ok_or_else(bad)?,
            action,
        };
        if !response
            && serde_json::from_value::<Plan>(row["plan"].clone()).map_err(|_| bad())? != plan
        {
            return Err(bad());
        }
        if unavailable {
            if !matches!(plan.action, Action::Stop { .. } | Action::Restart { .. })
                || e.is_none_or(|e| e > plan.deadline_ns)
            {
                return Err(bad());
            }
            expected.unavailable.insert(plan.id, e.ok_or_else(bad)?);
        } else if response {
            expected.effects.insert(plan.id, e.ok_or_else(bad)?);
        }
        expected.plans.push(plan);
    }
    Ok(expected)
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
    if request.control.is_none()
        && (!raw.control_history.is_empty() || raw.control_latency_ns.is_some_and(|n| n != 0))
    {
        return Err(Error::Integrity("unexpected control transcript".into()));
    }
    let expected = if request.control.is_some() {
        validate_control_history(
            &raw.control_history,
            raw.control_latency_ns,
            request,
            raw.fault_error.is_some(),
        )?
    } else {
        ValidatedControlHistory::default()
    };
    let mut faults = Vec::new();
    let mut fault_failure = raw.fault_error;
    if raw.faults.len() != expected.plans.len()
        && !(request.control.is_some() && fault_failure.is_some())
    {
        fault_failure = Some("fault request history is incomplete".to_string());
    }
    for (i, fault) in raw.faults.iter().enumerate() {
        let plan = expected
            .plans
            .get(i)
            .or_else(|| {
                (request.control.is_some() && fault_failure.is_some()).then_some(&fault.request)
            })
            .ok_or_else(|| Error::Integrity("unexpected fault acknowledgement".into()))?;
        if fault.request != *plan
            || fault.id as usize != i + 1
            || fault.requested_at_ns != plan.at_ns
            || fault.action != plan.action.name()
            || fault.machine.as_deref() != plan.action.machine()
        {
            return Err(Error::Integrity(
                "fault acknowledgement correlation mismatch".into(),
            ));
        }
        match (fault.status.as_str(), fault.applied_at_ns) {
            // A target VM exit after admission resolves the call with no effect,
            // at the time the target's reported clock stopped.
            ("unavailable", None)
                if expected.unavailable.get(&fault.id).is_some_and(|exit| {
                    raw.machines.iter().any(|machine| {
                        Some(&machine.name) == fault.machine.as_ref()
                            && machine.state.as_deref().is_some_and(|state| {
                                state
                                    .strip_prefix("Exited(")
                                    .and_then(|code| code.strip_suffix(')'))
                                    .and_then(|code| code.parse::<u8>().ok())
                                    .is_some_and(|code| state == format!("Exited({code})"))
                            })
                            && machine.time_ns == Some(*exit)
                    })
                }) => {}
            _ if expected.unavailable.contains_key(&fault.id) => {
                return Err(Error::Integrity("invalid fault effect status/time".into()))
            }
            ("applied", Some(at))
                if at >= fault.requested_at_ns
                    && at <= plan.deadline_ns
                    && (request.control.is_none()
                        || expected.effects.get(&fault.id).is_none_or(|e| *e == at)) =>
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
                    .unwrap_or_default();
                faults.push(crate::execution::FaultEvent {
                    requested_at_ns: fault.requested_at_ns,
                    applied_at_ns: at,
                    service,
                    action: fault.action.clone(),
                    detail: format!("id={} {}", fault.id, fault.detail),
                });
            }
            ("requested" | "unrequested", None) if !expected.effects.contains_key(&fault.id) => {}
            ("failed", None) => {
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
        backend.validate_retained(request.prepared)?;
        backend.preflight(Some(request.output))?;
        if request.prepared.profile != backend.profile {
            return Err(Error::Integrity("execution profile mismatch".into()));
        }
        if backend
            .cpu
            .is_some_and(|cpu| cpu >= libc::CPU_SETSIZE as u32)
        {
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
        let static_evidence = backend
            .admission
            .lock()
            .map_err(|_| Error::Execution("host admission evidence lock poisoned".into()))?
            .clone();
        fs::write(
            request.output.join("host-static.json"),
            serde_json::to_vec_pretty(&static_evidence)?,
        )?;
        let (cpu, mut lease) =
            HostLease::acquire_checked(backend.cpu, request.output, |reservation| {
                let mut observed = Vec::new();
                for cpu in &reservation.cpus {
                    match crate::admission::observe(*cpu) {
                        Ok(observation) => observed.push(observation),
                        Err(error) => {
                            fs::write(
                                request.output.join("host-leased.json"),
                                serde_json::to_vec_pretty(&observed)?,
                            )?;
                            fs::write(
                                request.output.join("host-leased-error.txt"),
                                error.to_string(),
                            )?;
                            return Err(error);
                        }
                    }
                }
                fs::write(
                    request.output.join("host-leased.json"),
                    serde_json::to_vec_pretty(&observed)?,
                )?;
                for observation in &observed {
                    crate::admission::validate_observation(observation)?;
                }
                Ok(())
            })?;
        let originals = lease.original_registers();
        fs::write(
            request.output.join("host-workaround.json"),
            serde_json::to_vec_pretty(&lease.enabled_registers()?)?,
        )?;
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
        fs::write(
            request.output.join("host-restored.json"),
            serde_json::to_vec_pretty(&originals)?,
        )?;
        completion
    }
}
