//! Engine report parsing and validation.

use super::*;

#[derive(Deserialize)]
pub(super) struct RawFault {
    request: seedbox_control::Plan,
    id: u32,
    requested_at_ns: u64,
    applied_at_ns: Option<u64>,
    action: String,
    machine: Option<String>,
    status: String,
    detail: String,
}

/// State parts emitted by `Machine::state_digest`: `events` is descriptive
/// text, while every other part is a lowercase SHA-256 digest. Keep unknown
/// part names digest-checked too, so new hashed parts remain safe to compare.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub(crate) struct StateDigestParts(BTreeMap<String, String>);

impl StateDigestParts {
    pub(crate) fn ram_only_difference(&self, other: &Self) -> bool {
        self.0
            .get("ram")
            .zip(other.0.get("ram"))
            .is_some_and(|(a, b)| a != b)
            && self
                .0
                .iter()
                .filter(|(k, _)| k.as_str() != "ram")
                .eq(other.0.iter().filter(|(k, _)| k.as_str() != "ram"))
    }
}

impl TryFrom<BTreeMap<String, String>> for StateDigestParts {
    type Error = Error;

    fn try_from(parts: BTreeMap<String, String>) -> Result<Self> {
        for (name, value) in &parts {
            if name != "events" {
                ContentDigest::try_from(value.clone())?;
            }
        }
        Ok(Self(parts))
    }
}

#[derive(Deserialize)]
pub(crate) struct MachineReport {
    pub(crate) name: String,
    pub(crate) infrastructure_error: bool,
    pub(crate) guest_exit_code: Option<u8>,
    pub(crate) guest_init_error: Option<u8>,
    pub(crate) state_digest: Option<ContentDigest>,
    pub(crate) state_digest_parts: Option<StateDigestParts>,
    pub(crate) witness: Option<ContentDigest>,
    pub(crate) branches: Option<u64>,
    pub(crate) pending_io: Option<Value>,
    /// Final machine state, `Exited(code)` once the VM has exited.
    #[serde(default)]
    pub(crate) state: Option<String>,
    /// Final simulated time; an exited VM's clock stops at its exit.
    #[serde(default)]
    pub(crate) time_ns: Option<u64>,
    #[serde(default)]
    crashed_incarnations: Vec<CrashedIncarnation>,
}
#[derive(Deserialize)]
pub(super) struct CrashedIncarnation {
    crashed_at_ns: u64,
}
#[derive(Deserialize)]
pub(crate) struct VmmReport {
    #[serde(default)]
    checkpoint: Option<Value>,
    /// Older engines always used counter.
    timebase: Option<String>,
    #[serde(default)]
    deferred_tx: bool,
    pub(crate) report_version: u32,
    seed: u64,
    /// Absent from engines older than counter profiles, which are all Zen.
    counter_profile: Option<String>,
    outcome: String,
    pub(crate) world_witness: ContentDigest,
    pub(crate) machines: Vec<MachineReport>,
    #[serde(default)]
    faults: Vec<RawFault>,
    #[serde(default)]
    fault_error: Option<String>,
    #[serde(default)]
    control_history: Vec<Value>,
    #[serde(default)]
    control_latency_ns: Option<u64>,
    #[serde(default)]
    triggers: Option<Vec<seedbox_control::TriggerRecord>>,
}

#[derive(Default)]
pub(super) struct ValidatedControlHistory {
    plans: Vec<seedbox_control::Plan>,
    effects: BTreeMap<u32, u64>,
    /// Process faults resolved without an effect at a VM exit or crash, or
    /// admitted while an earlier lifecycle plan left their target down.
    unavailable: BTreeMap<u32, u64>,
    unavailable_while_down: BTreeSet<u32>,
}

/// Correlate guest-originated requests to responses and applied engine actions.
pub(super) fn validate_control_history(
    history: &[Value],
    latency: Option<u64>,
    request: &ExecutionRequest<'_>,
    failed: bool,
    trigger_plans: Vec<seedbox_control::Plan>,
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
    let mut next_fault_id = 1u32;
    let mut down = BTreeSet::new();
    let mut clocks = BTreeSet::new();
    for (i, row) in history.iter().enumerate() {
        if row["status"] == "rejected" && failed {
            continue;
        }
        while trigger_plans.iter().any(|plan| plan.id == next_fault_id) {
            next_fault_id = next_fault_id.checked_add(1).ok_or_else(bad)?;
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
                if metadata["error"]["code"] == "invalid_state"
                    && (!row["fault_id"].is_null() || e != a)
                {
                    return Err(bad());
                }
                if row["fault_id"].is_null() || metadata["error"]["code"] != "target_unavailable" {
                    continue;
                }
                if row["fault_id"].as_u64() != Some(u64::from(next_fault_id)) {
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
            if !admitted || row["fault_id"].as_u64() != Some(u64::from(next_fault_id)) {
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
            Operation::PauseProcess { target } => Action::PauseProcess {
                machine: resolve(&target)?,
            },
            Operation::ResumeProcess { target } => Action::ResumeProcess {
                machine: resolve(&target)?,
            },
            Operation::Pause { target } => Action::Pause {
                machine: resolve(&target)?,
            },
            Operation::Resume { target } => Action::Resume {
                machine: resolve(&target)?,
            },
            Operation::Crash { target } => Action::Crash {
                machine: resolve(&target)?,
            },
            Operation::Reboot { target } => Action::Reboot {
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
            Operation::Shape {
                from,
                to,
                latency_ns,
                jitter_ns,
                loss_ppm,
                bandwidth_bps,
                queue_bound_ns,
                network_faults,
            } => {
                let handle = if response {
                    &metadata["result"]["handle"]
                } else {
                    &row["plan"]["handle"]
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty()
                    || (response && e != Some(a))
                    || latency_ns < request.network.latency_ns
                    || jitter_ns >= latency_ns
                    || latency_ns
                        .checked_add(jitter_ns)
                        .is_none_or(|n| n > 3_600_000_000_000)
                    || loss_ppm > 1_000_000
                    || bandwidth_bps == Some(0)
                    || (queue_bound_ns.is_some() && bandwidth_bps.is_none())
                    || network_faults.validate(latency_ns, jitter_ns).is_err()
                {
                    return Err(bad());
                }
                let from: Vec<_> = from.iter().map(&resolve).collect::<Result<_>>()?;
                let to: Vec<_> = to.iter().map(&resolve).collect::<Result<_>>()?;
                if from.is_empty()
                    || to.is_empty()
                    || from.iter().chain(&to).collect::<BTreeSet<_>>().len()
                        != from.len() + to.len()
                {
                    return Err(bad());
                }
                Action::Shape {
                    handle,
                    from,
                    to,
                    latency_ns,
                    jitter_ns,
                    loss_ppm,
                    bandwidth_bps,
                    queue_bound_ns,
                    network_faults,
                }
            }
            Operation::DropConnection { connection } => {
                let handle = if response {
                    &metadata["result"]["handle"]
                } else {
                    &row["plan"]["handle"]
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty()
                    || (response && e != Some(a))
                    || connection.incarnation == 0
                    || connection.src_port == 0
                    || connection.dst_port == 0
                    || resolve(&seedbox_control::Target::Machine(connection.from.clone()))?
                        == resolve(&seedbox_control::Target::Machine(connection.to.clone()))?
                {
                    return Err(bad());
                }
                Action::DropConnection { handle, connection }
            }
            Operation::Connections { from, to, .. } => {
                if e != Some(a) || resolve(&from)? == resolve(&to)? {
                    return Err(bad());
                }
                continue;
            }
            Operation::ClockSkew {
                target,
                offset_ns,
                max_offset_ns,
            } => {
                // An unavailable reply keeps its allocated handle in the error.
                let handle = match (response, unavailable) {
                    (true, false) => &metadata["result"]["handle"],
                    (true, true) => &metadata["error"]["handle"],
                    (false, _) => &row["plan"]["handle"],
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty()
                    || seedbox_control::validate_clock_skew(offset_ns, max_offset_ns).is_err()
                {
                    return Err(bad());
                }
                Action::ClockSkew {
                    handle,
                    machine: resolve(&target)?,
                    offset_ns,
                    max_offset_ns,
                }
            }
            Operation::Throttle {
                target,
                factor_permille,
            } => {
                let handle = if response {
                    &metadata["result"]["handle"]
                } else {
                    &row["plan"]["handle"]
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty()
                    || (response && e != Some(a))
                    || !seedbox_control::THROTTLE_PERMILLE.contains(&factor_permille)
                {
                    return Err(bad());
                }
                Action::Throttle {
                    handle,
                    machine: resolve(&target)?,
                    factor_permille,
                }
            }
            Operation::Heal { handle } => {
                // A clock heal waits for its guest effect; other heals apply at admission.
                if response && e != Some(a) && !clocks.contains(&handle) {
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
            Operation::ArmTrigger { .. } | Operation::CancelTrigger { .. } => {
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
        let lifecycle = matches!(
            action,
            Action::Pause { .. }
                | Action::Resume { .. }
                | Action::Crash { .. }
                | Action::Reboot { .. }
        );
        if lifecycle && (response || e.is_some()) && e != Some(a) {
            return Err(bad());
        }
        let plan = Plan {
            id: next_fault_id,
            at_ns: a,
            deadline_ns: a.checked_add(PROCESS_EFFECT_TIMEOUT_NS).ok_or_else(bad)?,
            action,
        };
        if !response
            && serde_json::from_value::<Plan>(row["plan"].clone()).map_err(|_| bad())? != plan
        {
            return Err(bad());
        }
        if let Action::ClockSkew { handle, .. } = &plan.action {
            clocks.insert(handle.clone());
        }
        if unavailable {
            if !(plan.action.is_process()
                || matches!(plan.action, Action::ClockSkew { .. } | Action::Heal { .. }))
                || e.is_none_or(|e| e > plan.deadline_ns)
            {
                return Err(bad());
            }
            expected.unavailable.insert(plan.id, e.ok_or_else(bad)?);
            if e == Some(a) && plan.action.machine().is_some_and(|m| down.contains(m)) {
                expected.unavailable_while_down.insert(plan.id);
            }
        } else if response {
            expected.effects.insert(plan.id, e.ok_or_else(bad)?);
        }
        if !unavailable && e == Some(a) {
            match &plan.action {
                Action::Crash { machine } => {
                    down.insert(machine.clone());
                }
                Action::Reboot { machine } => {
                    down.remove(machine);
                }
                _ => {}
            }
        }
        expected.plans.push(plan);
        next_fault_id = next_fault_id.checked_add(1).ok_or_else(bad)?;
    }
    expected.plans.extend(trigger_plans);
    expected.plans.sort_by_key(|p| p.id);
    if expected
        .plans
        .iter()
        .enumerate()
        .any(|(i, p)| p.id as usize != i + 1)
    {
        return Err(bad());
    }
    Ok(expected)
}

/// Check the bounded semantic trigger records against arm/cancel RPCs and
/// derive the sole fault each committed match may schedule. The engine owns
/// commitment; this validator rejects malformed or uncorrelated evidence.
pub(super) fn validate_triggers(
    records: Option<&[seedbox_control::TriggerRecord]>,
    history: &[Value],
    request: &ExecutionRequest<'_>,
) -> Result<Vec<seedbox_control::Plan>> {
    use seedbox_control::{
        Action, Operation, Plan, Request, Target, TriggerFault, MAX_TRIGGERS, MAX_TRIGGER_MARKER,
        PROCESS_EFFECT_TIMEOUT_NS,
    };
    let bad = || Error::Integrity("invalid committed trigger evidence".into());
    if records.is_some_and(|r| r.is_empty() || r.len() > MAX_TRIGGERS) {
        return Err(bad());
    }
    let records = records.unwrap_or_default();
    let latency = control_latency(request)?;
    let controller = request
        .control
        .and_then(|c| request.prepared.placement.get(&c.service));
    let resolve = |target: &Target, fault: bool| -> Result<String> {
        let name = match target {
            Target::Service(s) => request.prepared.placement.get(s).ok_or_else(bad)?,
            Target::Machine(m) => m,
        };
        if !request.prepared.guests.contains_key(name) || (fault && Some(name) == controller) {
            return Err(bad());
        }
        Ok(name.clone())
    };
    let mut arms = 0;
    let mut cancellations = BTreeMap::new();
    for row in history {
        let metadata = if row["response"].is_object() {
            &row["response"]
        } else {
            row
        };
        let accepted = if row["response"].is_object() {
            metadata["ok"] == true
        } else {
            row["admitted_done"] == true && row["error"].is_null()
        };
        if !accepted {
            continue;
        }
        let op = row["request"]["op"].as_str().unwrap_or_default();
        if op != "arm_trigger" && op != "cancel_trigger" {
            continue;
        }
        let call: Request = serde_json::from_value(row["request"].clone()).map_err(|_| bad())?;
        let at = metadata["admitted_at_ns"].as_u64().ok_or_else(bad)?;
        match call.operation {
            Operation::CancelTrigger { trigger_id } => {
                if trigger_id == 0
                    || trigger_id as usize > arms
                    || cancellations.insert(trigger_id, at).is_some()
                {
                    return Err(bad());
                }
            }
            Operation::ArmTrigger {
                source,
                marker,
                match_timeout_ns,
                fault,
            } => {
                let r = records.get(arms).ok_or_else(bad)?;
                arms += 1;
                if r.id as usize != arms
                    || metadata["result"]["trigger_id"].as_u64() != Some(u64::from(r.id))
                    || r.source != resolve(&source, false)?
                    || r.marker != marker
                    || marker.is_empty()
                    || marker.len() > MAX_TRIGGER_MARKER
                    || match_timeout_ns == 0
                    || r.armed_at_ns != at
                    || at.checked_add(match_timeout_ns) != Some(r.match_deadline_ns)
                {
                    return Err(bad());
                }
                let action = match fault {
                    TriggerFault::Stop { target } => Action::Stop {
                        machine: resolve(&target, true)?,
                    },
                    TriggerFault::Partition { left, right } => {
                        let handle = metadata["result"]["handle"]
                            .as_str()
                            .filter(|h| !h.is_empty())
                            .ok_or_else(bad)?
                            .to_owned();
                        let left = left
                            .iter()
                            .map(|t| resolve(t, true))
                            .collect::<Result<Vec<_>>>()?;
                        let right = right
                            .iter()
                            .map(|t| resolve(t, true))
                            .collect::<Result<Vec<_>>>()?;
                        let unique: BTreeSet<_> = left.iter().chain(&right).collect();
                        if left.is_empty()
                            || right.is_empty()
                            || unique.len() != left.len() + right.len()
                        {
                            return Err(bad());
                        }
                        Action::Partition {
                            handle,
                            left,
                            right,
                        }
                    }
                };
                if r.action != action {
                    return Err(bad());
                }
            }
            _ => return Err(bad()),
        }
    }
    if arms != records.len() {
        return Err(bad());
    }
    let mut plans = Vec::new();
    for r in records {
        let matched = r.matched_at_ns.is_some();
        if r.prefix > r.marker.len()
            || r.seen < r.prefix as u64
            || r.match_deadline_ns
                .checked_add(latency)
                .and_then(|d| d.checked_add(PROCESS_EFFECT_TIMEOUT_NS))
                .is_none()
            || r.effect_at_ns.is_some() != matched
            || (matched && r.prefix != r.marker.len())
            || (!matched && r.prefix == r.marker.len())
            || r.cancelled_at_ns != cancellations.get(&r.id).copied()
            || (r.status != "rejected" && r.rejection_reason.is_some())
        {
            return Err(bad());
        }
        if let Some(m) = r.matched_at_ns {
            if m < r.armed_at_ns
                || m >= r.match_deadline_ns
                || m.checked_add(latency) != r.effect_at_ns
            {
                return Err(bad());
            }
        }
        if r.cancelled_at_ns.is_some_and(|c| {
            c < r.armed_at_ns || c > r.match_deadline_ns || c >= request.limits.simulated_time_ns
        }) || r
            .cut_off_at_ns
            .is_some_and(|c| c < r.armed_at_ns || c > request.limits.simulated_time_ns)
        {
            return Err(bad());
        }
        match r.status.as_str() {
            "rejected"
                if matched
                    && r.cancelled_at_ns.is_none()
                    && r.cut_off_at_ns.is_none()
                    && r.fault_id.is_none()
                    && r.effect_at_ns
                        .is_some_and(|e| e < request.limits.simulated_time_ns)
                    && r.rejection_reason
                        .as_ref()
                        .is_some_and(|reason| !reason.is_empty()) => {}
            "fired" if matched && r.cancelled_at_ns.is_none() && r.cut_off_at_ns.is_none() => {
                let at_ns = r.effect_at_ns.ok_or_else(bad)?;
                if at_ns >= request.limits.simulated_time_ns {
                    return Err(bad());
                }
                plans.push(Plan {
                    id: r.fault_id.ok_or_else(bad)?,
                    at_ns,
                    deadline_ns: at_ns
                        .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                        .ok_or_else(bad)?,
                    action: r.action.clone(),
                });
            }
            "expired"
                if !matched
                    && r.cancelled_at_ns.is_none()
                    && r.cut_off_at_ns.is_none()
                    && r.match_deadline_ns < request.limits.simulated_time_ns
                    && r.fault_id.is_none() => {}
            "cancelled"
                if !matched
                    && r.cancelled_at_ns.is_some()
                    && r.cut_off_at_ns.is_none()
                    && r.fault_id.is_none() => {}
            "cut_off"
                if r.cancelled_at_ns.is_none()
                    && r.fault_id.is_none()
                    && r.cut_off_at_ns.is_some_and(|c| {
                        if matched {
                            r.effect_at_ns.is_some_and(|e| e >= c)
                        } else {
                            c <= r.match_deadline_ns
                        }
                    }) => {}
            _ => return Err(bad()),
        }
    }
    plans.sort_by_key(|p| p.at_ns);
    if plans.windows(2).any(|p| p[0].id >= p[1].id) {
        return Err(bad());
    }
    Ok(plans)
}

/// Parse the engine's structured completion report. Never infer application
/// exit status from log text or the VMM's infrastructure sentinel exit codes.
pub fn report(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let raw: VmmReport = serde_json::from_slice(bytes)?;
    if raw.report_version != 1 || raw.seed != request.seed {
        return Err(Error::Integrity("VMM report version/seed mismatch".into()));
    }
    let expected_checkpoint = if recorded_checkpoint(&request.prepared.profile)?.is_some() {
        world(request)?.get("checkpoint").cloned()
    } else {
        None
    };
    if raw.checkpoint != expected_checkpoint {
        return Err(Error::Integrity("VMM checkpoint identity mismatch".into()));
    }
    if raw.deferred_tx != recorded_deferred_tx(&request.prepared.profile)? {
        return Err(Error::Integrity(
            "VMM deferred_tx does not match recorded --deferred-tx".into(),
        ));
    }
    let timebase = recorded_timebase(&request.prepared.profile)?;
    let reported = raw.timebase.as_deref().unwrap_or("counter");
    if reported != timebase.name() {
        return Err(Error::Integrity(format!(
            "VMM --timebase {reported}: does not match recorded --timebase {}",
            timebase.name()
        )));
    }
    let counted = recorded_counter(&request.prepared.profile)?;
    if timebase == Timebase::Counter
        && counted != CounterProfile::Zen
        && raw.counter_profile.is_none()
    {
        return Err(Error::Integrity(
            "VMM report is missing its counter profile".into(),
        ));
    }
    let expected = counted.name();
    if raw
        .counter_profile
        .as_deref()
        .is_some_and(|name| name != expected)
    {
        return Err(Error::Integrity(format!(
            "VMM counted with a different branch counter profile than the recorded {expected}"
        )));
    }
    checked_report(raw, request)
}

/// The ARM adapter validates its own version, boot layout and PV identity
/// before sharing outcome/control correlation checks with the existing engine.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn report_hvf(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let raw: VmmReport = serde_json::from_slice(bytes)?;
    if raw.report_version != 2 || raw.seed != request.seed || raw.counter_profile.is_some() {
        return Err(Error::Integrity(
            "HVF application report version/seed/timebase mismatch".into(),
        ));
    }
    crate::mac::validate_report(bytes, request)?;
    checked_report(raw, request)
}

pub(super) fn checked_report(
    raw: VmmReport,
    request: &ExecutionRequest<'_>,
) -> Result<ExecutionReport> {
    let timebase = if crate::engine::recorded_launch(&request.prepared.profile)?
        == crate::engine::ExecutableKind::ArmWorld
    {
        Timebase::Pv
    } else {
        recorded_timebase(&request.prepared.profile)?
    };
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
        && (!raw.control_history.is_empty()
            || raw.control_latency_ns.is_some_and(|n| n != 0)
            || raw.triggers.is_some())
    {
        return Err(Error::Integrity("unexpected control transcript".into()));
    }
    let expected = if request.control.is_some() {
        validate_control_history(
            &raw.control_history,
            raw.control_latency_ns,
            request,
            raw.fault_error.is_some(),
            validate_triggers(raw.triggers.as_deref(), &raw.control_history, request)?,
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
            // Triggered stops have no RPC reply; unavailable effects retain
            // the same exit/crash window and prior-down evidence as process RPCs.
            ("unavailable", None)
                if raw.triggers.as_ref().is_some_and(|records| {
                    records.iter().any(|r| r.fault_id == Some(fault.id))
                }) && matches!(plan.action, seedbox_control::Action::Stop { .. })
                    && (raw.machines.iter().any(|m| {
                        Some(&m.name) == fault.machine.as_ref()
                            && ((m.state.as_deref().is_some_and(|s| {
                                s.strip_prefix("Exited(")
                                    .and_then(|c| c.strip_suffix(')'))
                                    .and_then(|c| c.parse::<u8>().ok())
                                    .is_some_and(|c| s == format!("Exited({c})"))
                            }) && m
                                .time_ns
                                .is_some_and(|t| t >= plan.at_ns && t <= plan.deadline_ns))
                                || m.crashed_incarnations.iter().any(|incarnation| {
                                    incarnation.crashed_at_ns >= plan.at_ns
                                        && incarnation.crashed_at_ns <= plan.deadline_ns
                                }))
                    }) || expected
                        .plans
                        .iter()
                        .take(i)
                        .rev()
                        .find(|prior| {
                            prior.action.machine() == plan.action.machine()
                                && matches!(
                                    prior.action,
                                    seedbox_control::Action::Crash { .. }
                                        | seedbox_control::Action::Reboot { .. }
                                )
                        })
                        .is_some_and(|prior| {
                            matches!(prior.action, seedbox_control::Action::Crash { .. })
                                && prior.at_ns <= plan.at_ns
                                && raw.faults.get(prior.id as usize - 1).is_some_and(|fault| {
                                    fault.status == "applied"
                                        && fault.applied_at_ns == Some(prior.at_ns)
                                })
                        })) => {}
            // An unavailable process fault needs a prior crash without reboot
            // at admission, or the target's exact exit/crash time in its window.
            ("unavailable", None)
                if expected.unavailable.get(&fault.id).is_some_and(|exit| {
                    expected.unavailable_while_down.contains(&fault.id)
                        || raw.machines.iter().any(|machine| {
                            Some(&machine.name) == fault.machine.as_ref()
                                && ((machine.state.as_deref().is_some_and(|state| {
                                    state
                                        .strip_prefix("Exited(")
                                        .and_then(|code| code.strip_suffix(')'))
                                        .and_then(|code| code.parse::<u8>().ok())
                                        .is_some_and(|code| state == format!("Exited({code})"))
                                }) && machine.time_ns == Some(*exit))
                                    || machine.crashed_incarnations.iter().any(|incarnation| {
                                        incarnation.crashed_at_ns == *exit
                                            && *exit >= plan.at_ns
                                            && *exit <= plan.deadline_ns
                                    }))
                        })
                }) => {}
            _ if expected.unavailable.contains_key(&fault.id) => {
                return Err(Error::Integrity("invalid fault effect status/time".into()))
            }
            ("applied", Some(at))
                if at >= fault.requested_at_ns
                    && at <= plan.deadline_ns
                    && (request.control.is_none()
                        || !matches!(
                            plan.action,
                            seedbox_control::Action::Pause { .. }
                                | seedbox_control::Action::Resume { .. }
                                | seedbox_control::Action::Crash { .. }
                                | seedbox_control::Action::Reboot { .. }
                        )
                        || at == plan.at_ns)
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
        timebase: Some(timebase),
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
