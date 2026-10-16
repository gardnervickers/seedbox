//! Engine report parsing and validation.

use super::*;
use seedbox_artifacts::evidence::StateDigestParts;
use seedbox_control::{
    Action, Header, Operation, Plan, Request, Target, TriggerFault, TriggerRecord, MAX_TRIGGERS,
    MAX_TRIGGER_MARKER, PROCESS_EFFECT_TIMEOUT_NS,
};
use seedbox_engine_protocol::report::RowStatus;
use seedbox_engine_protocol::report::RunOutcome;
use seedbox_engine_protocol::report::{
    ControlRecord, ControlStatus, EngineReport, FaultRecord, MachineOutcome, WorldEvidence,
};
use serde_json::Value;

fn external_program(
    request: &ExecutionRequest<'_>,
) -> Result<Vec<seedbox_engine_protocol::input_program::TimedInput>> {
    let history = match request.intervention_history {
        Some(path) => seedbox_artifacts::interventions::read_optional(path)?.unwrap_or_default(),
        None => Default::default(),
    };
    let mut program = request.program.to_vec();
    seedbox_engine_protocol::input_program::merge_interventions(&mut program, &history)
        .map_err(Error::Integrity)?;
    Ok(program)
}

/// Completed runs may stop before the end of a program. Every reported
/// admission must be an exact prefix of the authored document, with IDs
/// allocated in execution order rather than reserved by the document.
fn validate_input_admissions(
    program: &[seedbox_engine_protocol::input_program::TimedInput],
    admissions: &[seedbox_engine_protocol::input_program::InputAdmission],
    rejections: &[seedbox_engine_protocol::input_program::InputRejection],
    branches: &[seedbox_engine_protocol::input_program::BranchReceipt],
    armed: &[seedbox_engine_protocol::input_program::ArmReceipt],
    duration_ns: u64,
) -> Result<Vec<Plan>> {
    use seedbox_engine_protocol::input_program::InputAction;
    seedbox_engine_protocol::input_program::validate_program(program, duration_ns)
        .map_err(Error::Integrity)?;
    let consumed = admissions.len() + rejections.len() + branches.len() + armed.len();
    if consumed > program.len() {
        return Err(correlation_mismatch());
    }
    let mut admitted = admissions.iter().peekable();
    let mut rejected = rejections.iter().peekable();
    let mut branched = branches.iter();
    let mut armed = armed.iter();
    let mut previous_trigger = 0;
    let mut previous = 0;
    for input in program.iter().take(consumed) {
        if let InputAction::Branch(action) = &input.action {
            let receipt = branched.next().ok_or_else(correlation_mismatch)?;
            if receipt.key != input.key || receipt.at_ns != input.at_ns || &receipt.action != action
            {
                return Err(correlation_mismatch());
            }
            continue;
        }
        if rejected.peek().is_some_and(|r| r.key == input.key) {
            let row = rejected.next().ok_or_else(correlation_mismatch)?;
            if row.at_ns != input.at_ns || row.reason.is_empty() {
                return Err(correlation_mismatch());
            }
            continue;
        }
        if let InputAction::Rule(_) = &input.action {
            let receipt = armed.next().ok_or_else(correlation_mismatch)?;
            if receipt.key != input.key
                || receipt.at_ns != input.at_ns
                || receipt.trigger_id <= previous_trigger
            {
                return Err(correlation_mismatch());
            }
            previous_trigger = receipt.trigger_id;
            continue;
        }
        let row = admitted.next().ok_or_else(correlation_mismatch)?;
        let plan = &row.plan;
        if input.key != row.key
            || input.at_ns != plan.at_ns
            || input.action.fault() != Some(&plan.action)
            || plan.id <= previous
            || input.at_ns.checked_add(input.timeout_ns) != Some(plan.deadline_ns)
        {
            return Err(correlation_mismatch());
        }
        previous = plan.id;
    }
    Ok(admissions.iter().map(|entry| entry.plan.clone()).collect())
}

fn validate_program_cutoff(
    evidence: &WorldEvidence,
    scheduler: &Value,
    machines: &[MachineOutcome],
    request: &ExecutionRequest<'_>,
) -> Result<()> {
    if evidence.outcome != RunOutcome::FaultFailed {
        let program = external_program(request)?;
        if let Some(next) = program.get(
            evidence.input_admissions.len()
                + evidence.input_rejections.len()
                + evidence.input_branches.len()
                + evidence.input_armed.len(),
        ) {
            let committed = scheduler
                .get("committed_ns")
                .and_then(Value::as_u64)
                .ok_or_else(correlation_mismatch)?;
            // Oracle exits and checkpoints can stop at T before its inputs.
            let terminal_time = if evidence.outcome == RunOutcome::EndMachineExited {
                let oracle = request
                    .prepared
                    .placement
                    .get(&request.check.service)
                    .ok_or_else(correlation_mismatch)?;
                machines
                    .iter()
                    .find(|machine| &machine.name == oracle)
                    .filter(|machine| machine.guest_exit_code.is_some())
                    .map(|machine| machine.time_ns)
                    .ok_or_else(correlation_mismatch)?
                    .min(committed)
            } else {
                committed
            };
            if next.at_ns < terminal_time {
                return Err(correlation_mismatch());
            }
        }
    }
    Ok(())
}

/// The ARM engine's report, as far as the shared validation reads it.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[derive(Deserialize)]
struct HvfReport {
    report_version: u32,
    seed: u64,
    #[serde(flatten)]
    evidence: WorldEvidence,
    machines: Vec<MachineOutcome>,
    #[serde(default)]
    scheduler: Value,
}

#[derive(Default)]
pub(super) struct ValidatedControlHistory {
    plans: Vec<Plan>,
    interventions: BTreeSet<u32>,
    effects: BTreeMap<u32, u64>,
    /// Process faults resolved without an effect at a VM exit or crash, or
    /// admitted while an earlier lifecycle plan left their target down.
    unavailable: BTreeMap<u32, u64>,
    unavailable_while_down: BTreeSet<u32>,
}

fn correlation_mismatch() -> Error {
    Error::Integrity("control request/response/time correlation mismatch".into())
}

/// When a transcript row's request was admitted, took effect and was
/// answered, and when the run cut it off.
#[derive(Clone, Copy)]
struct RowTimes {
    admitted: u64,
    effect: Option<u64>,
    replied: Option<u64>,
    cut_off: Option<u64>,
}

/// The running state of a control transcript check.
struct HistoryCheck<'a> {
    request: &'a ExecutionRequest<'a>,
    latency: u64,
    failed: bool,
    external_plans: &'a [Plan],
    controller: Option<&'a String>,
    expected: ValidatedControlHistory,
    previous: u64,
    session: u64,
    public_id: u64,
    accepted: u64,
    next_fault_id: u32,
    /// Machines a crash has taken down and no reboot has brought back.
    down: BTreeSet<String>,
    /// Clock skew handles, whose heals wait for their guest effect.
    clocks: BTreeSet<String>,
}

/// Correlate guest-originated requests to responses and applied engine actions.
pub(super) fn validate_control_history(
    history: &[ControlRecord],
    latency: u64,
    request: &ExecutionRequest<'_>,
    failed: bool,
    external_plans: Vec<Plan>,
) -> Result<ValidatedControlHistory> {
    if latency != control_latency(request)? {
        return Err(Error::Integrity("control latency mismatch".into()));
    }
    let mut check = HistoryCheck {
        request,
        latency,
        failed,
        external_plans: &external_plans,
        controller: request
            .control
            .and_then(|control| request.prepared.placement.get(&control.service)),
        expected: ValidatedControlHistory::default(),
        previous: 0,
        session: 0,
        public_id: 0,
        accepted: 0,
        next_fault_id: 1,
        down: BTreeSet::new(),
        clocks: BTreeSet::new(),
    };
    for (i, row) in history.iter().enumerate() {
        if row.status == Some(ControlStatus::Rejected) && failed {
            continue;
        }
        check.row(row, i + 1 == history.len())?;
    }
    let mut expected = check.expected;
    expected.plans.extend(external_plans);
    expected.plans.sort_by_key(|p| p.id);
    if expected
        .plans
        .iter()
        .enumerate()
        .any(|(i, p)| p.id as usize != i + 1)
    {
        return Err(correlation_mismatch());
    }
    Ok(expected)
}

impl HistoryCheck<'_> {
    /// Check one accepted transcript row and record the fault it planned.
    fn row(&mut self, row: &ControlRecord, last: bool) -> Result<()> {
        let bad = correlation_mismatch;
        self.accepted += 1;
        let times = self.correlate(row, last)?;
        while self
            .external_plans
            .iter()
            .any(|p| p.id == self.next_fault_id)
        {
            self.next_fault_id = self.next_fault_id.checked_add(1).ok_or_else(bad)?;
        }
        // Only earlier admissions may affect this guest request's lifecycle.
        self.down.clear();
        self.clocks.clear();
        let mut prior: Vec<_> = self
            .external_plans
            .iter()
            .chain(self.expected.plans.iter())
            .filter(|p| p.id < self.next_fault_id && p.at_ns <= times.admitted)
            .collect();
        prior.sort_by_key(|p| p.id);
        for plan in prior {
            let effect_at_admission = self.external_plans.iter().any(|p| p.id == plan.id)
                || self.expected.effects.get(&plan.id) == Some(&plan.at_ns);
            match &plan.action {
                Action::Crash { machine } if effect_at_admission => {
                    self.down.insert(machine.clone());
                }
                Action::Reboot { machine } if effect_at_admission => {
                    self.down.remove(machine);
                }
                Action::ClockSkew { handle, .. } | Action::ClockDrift { handle, .. } => {
                    self.clocks.insert(handle.clone());
                }
                _ => {}
            }
        }
        if !self.plans_fault(row, times)? {
            return Ok(());
        }
        let unavailable = match &row.response {
            Some(reply) => !reply.ok,
            None => times.effect.is_some() && row.error.is_some() && row.fatal == Some(false),
        };
        let call: Request = serde_json::from_value(row.request.clone()).map_err(|_| bad())?;
        let Some(action) = self.action(row, call.operation, times, unavailable)? else {
            return Ok(());
        };
        self.record(row, action, times, unavailable)
    }

    /// The row's session, relay and request ids and its times must follow
    /// the previous row's.
    fn correlate(&mut self, row: &ControlRecord, last: bool) -> Result<RowTimes> {
        let bad = correlation_mismatch;
        let current_session = row.session_id.ok_or_else(bad)?;
        if current_session == 0
            || current_session > u32::MAX as u64
            || current_session < self.session
        {
            return Err(bad());
        }
        self.public_id = if current_session == self.session {
            self.public_id + 1
        } else {
            1
        };
        self.session = current_session;
        let timing = row.timing();
        let requested = timing.requested_at_ns.ok_or_else(bad)?;
        let admitted = timing.admitted_at_ns.ok_or_else(bad)?;
        let cut_off = match row.status {
            Some(ControlStatus::CutOff) => Some(row.cut_off_at_ns.ok_or_else(bad)?),
            _ => None,
        };
        let header = Header::deserialize(&row.request).ok();
        let limit = self.request.limits.simulated_time_ns;
        if row.relay_id != Some(self.accepted)
            || header.as_ref().map(|h| h.version) != Some(1)
            || header.as_ref().map(|h| u64::from(h.id.get())) != Some(self.public_id)
            || self.public_id > u32::MAX as u64
            || self.accepted > u32::MAX as u64
            || requested < self.previous
            || requested.checked_add(self.latency) != Some(admitted)
            || cut_off.is_some_and(|c| c < requested || c > limit || !last)
        {
            return Err(bad());
        }
        let (effect, replied) = (timing.effect_at_ns, timing.replied_at_ns);
        if effect.is_some_and(|e| e < admitted)
            || replied.is_some_and(|r| effect.and_then(|e| e.checked_add(self.latency)) != Some(r))
        {
            return Err(bad());
        }
        Ok(RowTimes {
            admitted,
            effect,
            replied,
            cut_off,
        })
    }

    /// Check the row's reply or cut-off state. False when the request
    /// planned no fault: an error reply, or a cut-off request with no fault.
    fn plans_fault(&mut self, row: &ControlRecord, times: RowTimes) -> Result<bool> {
        let bad = correlation_mismatch;
        let RowTimes {
            admitted: a,
            effect: e,
            cut_off,
            ..
        } = times;
        let fault_id = row.fault_id;
        let next = Some(u64::from(self.next_fault_id));
        let Some(reply) = &row.response else {
            let c = cut_off.ok_or_else(bad)?;
            let admitted = row.admitted_done.ok_or_else(bad)?;
            if (admitted && a > c) || (!admitted && a < c) {
                return Err(bad());
            }
            if fault_id.is_none() {
                if row.plan.is_some() && admitted {
                    return Err(bad());
                }
                return Ok(false);
            }
            if !admitted || fault_id != next {
                return Err(bad());
            }
            return Ok(true);
        };
        let e = e.ok_or_else(bad)?;
        let replied = times.replied.ok_or_else(bad)?;
        let header_id = Header::deserialize(&row.request).map(|h| u64::from(h.id.get()));
        if reply.version != 1
            || header_id.ok() != Some(reply.id)
            || e.checked_add(self.latency) != Some(replied)
            || (cut_off.is_none() && replied >= self.request.limits.simulated_time_ns)
            || if cut_off.is_some() {
                row.delivered_at_ns.is_some()
            } else {
                row.delivered_at_ns != Some(replied) && !self.failed
            }
        {
            return Err(bad());
        }
        self.previous = replied;
        if !reply.ok {
            let error = reply
                .error
                .as_ref()
                .ok_or_else(|| Error::Integrity("invalid control error reply".into()))?;
            if error.code == "invalid_state" && (fault_id.is_some() || e != a) {
                return Err(bad());
            }
            if fault_id.is_none() || error.code != "target_unavailable" {
                return Ok(false);
            }
            if fault_id != next {
                return Err(bad());
            }
        }
        Ok(true)
    }

    /// The machine a fault target names; never the controller.
    fn resolve(&self, target: &Target) -> Result<String> {
        let machine = match target {
            Target::Service(service) => self
                .request
                .prepared
                .placement
                .get(service)
                .map(String::as_str),
            Target::Machine(machine) => Some(machine.as_str()),
        }
        .ok_or_else(|| Error::Integrity("invalid control target".into()))?;
        if !self.request.prepared.guests.contains_key(machine)
            || self.controller.is_some_and(|name| name == machine)
        {
            return Err(Error::Integrity(
                "invalid fault machine/controller target".into(),
            ));
        }
        Ok(machine.to_owned())
    }

    fn resolve_all(&self, targets: &[Target]) -> Result<Vec<String>> {
        targets.iter().map(|t| self.resolve(t)).collect()
    }

    /// The fault action a request planned, or `None` for an operation that
    /// plans no fault.
    fn action(
        &self,
        row: &ControlRecord,
        operation: Operation,
        times: RowTimes,
        unavailable: bool,
    ) -> Result<Option<Action>> {
        let bad = correlation_mismatch;
        let response = row.response.is_some();
        let (a, e) = (times.admitted, times.effect);
        // A handle allocated at admission, applied at once when answered.
        let handle = || -> Result<String> {
            let handle = row.planned_handle().ok_or_else(bad)?;
            if handle.is_empty() || (response && e != Some(a)) {
                return Err(bad());
            }
            Ok(handle.to_owned())
        };
        let action = match operation {
            Operation::Stop { target } => Action::Stop {
                machine: self.resolve(&target)?,
            },
            Operation::Restart { target } => Action::Restart {
                machine: self.resolve(&target)?,
            },
            Operation::PauseProcess { target } => Action::PauseProcess {
                machine: self.resolve(&target)?,
            },
            Operation::ResumeProcess { target } => Action::ResumeProcess {
                machine: self.resolve(&target)?,
            },
            Operation::Pause { target } => Action::Pause {
                machine: self.resolve(&target)?,
            },
            Operation::Resume { target } => Action::Resume {
                machine: self.resolve(&target)?,
            },
            Operation::Crash { target } => Action::Crash {
                machine: self.resolve(&target)?,
            },
            Operation::Reboot { target } => Action::Reboot {
                machine: self.resolve(&target)?,
            },
            Operation::Partition { left, right } => Action::Partition {
                handle: handle()?,
                left: self.resolve_all(&left)?,
                right: self.resolve_all(&right)?,
            },
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
                let handle = handle()?;
                if latency_ns < self.request.network.latency_ns
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
                let from = self.resolve_all(&from)?;
                let to = self.resolve_all(&to)?;
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
                let handle = handle()?;
                if connection.incarnation == 0
                    || connection.src_port == 0
                    || connection.dst_port == 0
                    || self.resolve(&Target::Machine(connection.from.clone()))?
                        == self.resolve(&Target::Machine(connection.to.clone()))?
                {
                    return Err(bad());
                }
                Action::DropConnection { handle, connection }
            }
            Operation::Connections { from, to, .. } => {
                if e != Some(a) || self.resolve(&from)? == self.resolve(&to)? {
                    return Err(bad());
                }
                return Ok(None);
            }
            Operation::ClockSkew {
                target,
                offset_ns,
                max_offset_ns,
            } => {
                // An unavailable reply keeps its allocated handle in the error.
                let handle = match (&row.response, unavailable) {
                    (Some(reply), true) => reply.error.as_ref().and_then(|e| e.handle.as_deref()),
                    _ => row.planned_handle(),
                }
                .ok_or_else(bad)?
                .to_owned();
                if handle.is_empty()
                    || seedbox_control::validate_clock_skew(offset_ns, max_offset_ns).is_err()
                {
                    return Err(bad());
                }
                Action::ClockSkew {
                    handle,
                    machine: self.resolve(&target)?,
                    offset_ns,
                    max_offset_ns,
                }
            }
            Operation::ClockDrift {
                target,
                rate_ppb,
                max_rate_ppb,
            } => {
                // An unavailable reply keeps its allocated handle in the error.
                let handle = match (&row.response, unavailable) {
                    (Some(reply), true) => reply.error.as_ref().and_then(|e| e.handle.as_deref()),
                    _ => row.planned_handle(),
                }
                .ok_or_else(bad)?
                .to_owned();
                if handle.is_empty()
                    || seedbox_control::validate_clock_drift(rate_ppb, max_rate_ppb).is_err()
                {
                    return Err(bad());
                }
                Action::ClockDrift {
                    handle,
                    machine: self.resolve(&target)?,
                    rate_ppb,
                    max_rate_ppb,
                }
            }
            Operation::Throttle { target, speed_hz } => {
                let handle = handle()?;
                if !seedbox_control::THROTTLE_SPEED_HZ.contains(&speed_hz) {
                    return Err(bad());
                }
                Action::Throttle {
                    handle,
                    machine: self.resolve(&target)?,
                    speed_hz,
                }
            }
            Operation::Heal { handle } => {
                // A clock heal waits for its guest effect; other heals apply at admission.
                if response && e != Some(a) && !self.clocks.contains(&handle) {
                    return Err(bad());
                }
                Action::Heal { handle }
            }
            Operation::Now {} => {
                let now = row
                    .result()
                    .and_then(|r| r.get("now_ns"))
                    .and_then(Value::as_u64);
                if e != Some(a) || now != e {
                    return Err(bad());
                }
                return Ok(None);
            }
            Operation::Inspect {}
            | Operation::ArmTrigger { .. }
            | Operation::CancelTrigger { .. } => {
                if e != Some(a) {
                    return Err(bad());
                }
                return Ok(None);
            }
            Operation::Sleep { duration_ns } => {
                if a.checked_add(duration_ns) != e {
                    return Err(bad());
                }
                return Ok(None);
            }
            // Quiet changes no machine; it answers at admission.
            Operation::Quiet { .. } => {
                if response && e != Some(a) {
                    return Err(bad());
                }
                return Ok(None);
            }
        };
        Ok(Some(action))
    }

    /// Record the fault plan of an accepted request and its expected effect.
    fn record(
        &mut self,
        row: &ControlRecord,
        action: Action,
        times: RowTimes,
        unavailable: bool,
    ) -> Result<()> {
        let bad = correlation_mismatch;
        let response = row.response.is_some();
        let (a, e) = (times.admitted, times.effect);
        if action.is_lifecycle() && (response || e.is_some()) && e != Some(a) {
            return Err(bad());
        }
        let plan = Plan {
            id: self.next_fault_id,
            at_ns: a,
            deadline_ns: a.checked_add(PROCESS_EFFECT_TIMEOUT_NS).ok_or_else(bad)?,
            action,
        };
        if !response {
            let planned: Plan = row
                .plan
                .as_ref()
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .ok_or_else(bad)?;
            if planned != plan {
                return Err(bad());
            }
        }
        if let Action::ClockSkew { handle, .. } | Action::ClockDrift { handle, .. } = &plan.action {
            self.clocks.insert(handle.clone());
        }
        let expected = &mut self.expected;
        if unavailable {
            if !(plan.action.is_process()
                || matches!(
                    plan.action,
                    Action::ClockSkew { .. } | Action::ClockDrift { .. } | Action::Heal { .. }
                ))
                || e.is_none_or(|e| e > plan.deadline_ns)
            {
                return Err(bad());
            }
            expected.unavailable.insert(plan.id, e.ok_or_else(bad)?);
            if e == Some(a) && plan.action.machine().is_some_and(|m| self.down.contains(m)) {
                expected.unavailable_while_down.insert(plan.id);
            }
        } else if response {
            expected.effects.insert(plan.id, e.ok_or_else(bad)?);
        }
        if !unavailable && e == Some(a) {
            match &plan.action {
                Action::Crash { machine } => {
                    self.down.insert(machine.clone());
                }
                Action::Reboot { machine } => {
                    self.down.remove(machine);
                }
                _ => {}
            }
        }
        expected.plans.push(plan);
        self.next_fault_id = self.next_fault_id.checked_add(1).ok_or_else(bad)?;
        Ok(())
    }
}

fn invalid_trigger_evidence() -> Error {
    Error::Integrity("invalid committed trigger evidence".into())
}

/// Check the bounded semantic trigger records against arm/cancel RPCs and
/// derive the sole fault each committed match may schedule. The engine owns
/// commitment; this validator rejects malformed or uncorrelated evidence.
pub(super) fn validate_triggers(
    records: Option<&[TriggerRecord]>,
    history: &[ControlRecord],
    request: &ExecutionRequest<'_>,
    program: &[seedbox_engine_protocol::input_program::TimedInput],
    armed: &[seedbox_engine_protocol::input_program::ArmReceipt],
) -> Result<Vec<Plan>> {
    let bad = invalid_trigger_evidence;
    if records.is_some_and(|r| r.is_empty() || r.len() > MAX_TRIGGERS) {
        return Err(bad());
    }
    let records = records.unwrap_or_default();
    if records
        .iter()
        .enumerate()
        .any(|(i, r)| r.id as usize != i + 1)
    {
        return Err(bad());
    }
    let (typed, marker): (Vec<_>, Vec<_>) = records.iter().partition(|r| r.selector.is_some());
    if request.control.is_none() && !marker.is_empty() {
        return Err(Error::Integrity("unexpected control transcript".into()));
    }
    let latency = if marker.is_empty() {
        0
    } else {
        control_latency(request)?
    };
    let cancellations = correlate_trigger_requests(&marker, history, request)?;
    correlate_typed_triggers(&typed, program, armed)?;
    // Typed effects follow their match by the engine's causal lookahead,
    // one positive value per run that no link latency undercuts.
    let lookahead = typed
        .iter()
        .find_map(|r| {
            Some(
                r.effect_at_ns?
                    .checked_sub(r.matched_at_ns?)?
                    .checked_sub(r.delay_ns),
            )
        })
        .map(|d| d.filter(|d| *d > 0 && *d <= request.network.latency_ns))
        .map_or(Ok(0), |d| d.ok_or_else(bad))?;
    let mut plans = Vec::new();
    for r in records {
        let latency = if r.selector.is_some() {
            lookahead
        } else {
            latency
        };
        if let Some(plan) = trigger_fault(r, &cancellations, latency, request)? {
            plans.push(plan);
        }
    }
    plans.sort_by_key(|p| p.at_ns);
    if plans.windows(2).any(|p| p[0].id >= p[1].id) {
        return Err(bad());
    }
    Ok(plans)
}

/// Each typed trigger was armed by its input's receipt, exactly as authored.
fn correlate_typed_triggers(
    typed: &[&TriggerRecord],
    program: &[seedbox_engine_protocol::input_program::TimedInput],
    armed: &[seedbox_engine_protocol::input_program::ArmReceipt],
) -> Result<()> {
    use seedbox_engine_protocol::input_program::{InputAction, RuleAction};
    let bad = invalid_trigger_evidence;
    if typed.len() != armed.len() {
        return Err(bad());
    }
    for (r, receipt) in typed.iter().zip(armed) {
        let input = program
            .iter()
            .find(|input| input.key == receipt.key)
            .ok_or_else(bad)?;
        let InputAction::Rule(RuleAction::Trigger(spec)) = &input.action else {
            return Err(bad());
        };
        let deadline = match spec.within_ns {
            Some(within) => receipt.at_ns.checked_add(within),
            None => Some(u64::MAX),
        };
        if r.id != receipt.trigger_id
            || r.key.as_ref() != Some(&receipt.key)
            || r.armed_at_ns != receipt.at_ns
            || input.at_ns != receipt.at_ns
            || r.selector.as_ref() != Some(&spec.when)
            || r.nth != spec.nth
            || r.delay_ns != spec.delay_ns
            || r.action != spec.then
            || Some(r.match_deadline_ns) != deadline
            || !r.marker.is_empty()
            || r.prefix != 0
            || r.resets != 0
            || r.cancelled_at_ns.is_some()
        {
            return Err(bad());
        }
    }
    Ok(())
}

/// Match each accepted arm request to its record in order, and collect the
/// time each trigger was cancelled.
fn correlate_trigger_requests(
    records: &[&TriggerRecord],
    history: &[ControlRecord],
    request: &ExecutionRequest<'_>,
) -> Result<BTreeMap<u32, u64>> {
    let bad = invalid_trigger_evidence;
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
    let resolve_faults = |targets: &[Target]| -> Result<Vec<String>> {
        targets.iter().map(|t| resolve(t, true)).collect()
    };
    let mut arms = 0;
    let mut cancellations = BTreeMap::new();
    for row in history {
        let accepted = match &row.response {
            Some(reply) => reply.ok,
            None => row.admitted_done == Some(true) && row.error.is_none(),
        };
        let op = row.request.get("op").and_then(Value::as_str);
        if !accepted || !matches!(op, Some("arm_trigger" | "cancel_trigger")) {
            continue;
        }
        let call: Request = serde_json::from_value(row.request.clone()).map_err(|_| bad())?;
        let at = row.timing().admitted_at_ns.ok_or_else(bad)?;
        let result = |key: &str| row.result().and_then(|r| r.get(key));
        match call.operation {
            Operation::CancelTrigger { trigger_id } => {
                if !records[..arms].iter().any(|r| r.id == trigger_id)
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
                if result("trigger_id").and_then(Value::as_u64) != Some(u64::from(r.id))
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
                        let handle = result("handle")
                            .and_then(Value::as_str)
                            .filter(|h| !h.is_empty())
                            .ok_or_else(bad)?
                            .to_owned();
                        let left = resolve_faults(&left)?;
                        let right = resolve_faults(&right)?;
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
    Ok(cancellations)
}

/// Check one trigger record's life against its own times; a fired trigger
/// yields the fault it scheduled.
fn trigger_fault(
    r: &TriggerRecord,
    cancellations: &BTreeMap<u32, u64>,
    latency: u64,
    request: &ExecutionRequest<'_>,
) -> Result<Option<Plan>> {
    use seedbox_control::TriggerStatus;
    let bad = invalid_trigger_evidence;
    let limit = request.limits.simulated_time_ns;
    let matched = r.matched_at_ns.is_some();
    // A typed trigger completes on its nth occurrence; a marker on its last byte.
    let typed = r.selector.is_some();
    let complete = if typed {
        r.seen == r.nth
    } else {
        r.prefix == r.marker.len()
    };
    let unbounded = typed && r.match_deadline_ns == u64::MAX;
    if r.prefix > r.marker.len()
        || r.seen < r.prefix as u64
        || (typed && r.seen > r.nth)
        || (!unbounded
            && r.match_deadline_ns
                .checked_add(latency)
                .and_then(|d| d.checked_add(PROCESS_EFFECT_TIMEOUT_NS))
                .is_none())
        || r.effect_at_ns.is_some() != matched
        || matched != complete
        || r.cancelled_at_ns != cancellations.get(&r.id).copied()
        || (r.status != TriggerStatus::Rejected && r.rejection_reason.is_some())
    {
        return Err(bad());
    }
    if let Some(m) = r.matched_at_ns {
        if m < r.armed_at_ns
            || m >= r.match_deadline_ns
            || m.checked_add(latency)
                .and_then(|e| e.checked_add(r.delay_ns))
                != r.effect_at_ns
        {
            return Err(bad());
        }
    }
    if r.cancelled_at_ns
        .is_some_and(|c| c < r.armed_at_ns || c > r.match_deadline_ns || c >= limit)
        || r.cut_off_at_ns
            .is_some_and(|c| c < r.armed_at_ns || c > limit)
    {
        return Err(bad());
    }
    let ended = r.cancelled_at_ns.is_none() && r.cut_off_at_ns.is_none();
    match r.status {
        TriggerStatus::Rejected
            if matched
                && ended
                && r.fault_id.is_none()
                && r.effect_at_ns.is_some_and(|e| e < limit)
                && r.rejection_reason
                    .as_ref()
                    .is_some_and(|reason| !reason.is_empty()) =>
        {
            Ok(None)
        }
        TriggerStatus::Fired if matched && ended => {
            let at_ns = r.effect_at_ns.ok_or_else(bad)?;
            if at_ns >= limit {
                return Err(bad());
            }
            Ok(Some(Plan {
                id: r.fault_id.ok_or_else(bad)?,
                at_ns,
                deadline_ns: at_ns
                    .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                    .ok_or_else(bad)?,
                action: r.action.clone(),
            }))
        }
        TriggerStatus::Expired
            if !matched && ended && r.match_deadline_ns < limit && r.fault_id.is_none() =>
        {
            Ok(None)
        }
        TriggerStatus::Cancelled
            if !matched
                && r.cancelled_at_ns.is_some()
                && r.cut_off_at_ns.is_none()
                && r.fault_id.is_none() =>
        {
            Ok(None)
        }
        TriggerStatus::CutOff
            if r.cancelled_at_ns.is_none()
                && r.fault_id.is_none()
                && r.cut_off_at_ns.is_some_and(|c| {
                    if matched {
                        r.effect_at_ns.is_some_and(|e| e >= c)
                    } else {
                        c <= r.match_deadline_ns
                    }
                }) =>
        {
            Ok(None)
        }
        _ => Err(bad()),
    }
}

/// Parse the engine's structured completion report. Never infer application
/// exit status from log text or the VMM's infrastructure sentinel exit codes.
pub fn report(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let raw: EngineReport = serde_json::from_slice(bytes)?;
    if raw.report_version != 1 || raw.seed != request.seed {
        return Err(Error::Integrity("VMM report version/seed mismatch".into()));
    }
    let expected_checkpoint = if recorded_checkpoint(&request.prepared.profile)?.is_some() {
        world(request)?.checkpoint
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
    let machines: Vec<_> = raw.machines.into_iter().map(|m| m.outcome).collect();
    validate_program_cutoff(&raw.evidence, &raw.scheduler, &machines, request)?;
    let mut report = checked_report(raw.evidence, &machines, request)?;
    report.simulated_time_ns = raw
        .scheduler
        .get("committed_ns")
        .and_then(Value::as_u64)
        .or(report.simulated_time_ns);
    Ok(report)
}

/// The ARM adapter validates its own version, boot layout and PV identity
/// before sharing outcome/control correlation checks with the existing engine.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn report_hvf(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let raw: HvfReport = serde_json::from_slice(bytes)?;
    if raw.report_version != 2 || raw.seed != request.seed {
        return Err(Error::Integrity(
            "HVF application report version/seed mismatch".into(),
        ));
    }
    crate::mac::validate_report(bytes, request)?;
    validate_program_cutoff(&raw.evidence, &raw.scheduler, &raw.machines, request)?;
    let mut report = checked_report(raw.evidence, &raw.machines, request)?;
    report.simulated_time_ns = raw
        .scheduler
        .get("committed_ns")
        .and_then(Value::as_u64)
        .or(report.simulated_time_ns);
    Ok(report)
}

/// Whether `machine` left the run inside `[from, to]`: its VM exited then,
/// or one of its incarnations crashed then.
fn left_within(machine: &MachineOutcome, from: u64, to: u64) -> bool {
    let within = |t: u64| t >= from && t <= to;
    (machine.state.exited() && within(machine.time_ns))
        || machine
            .crashed_incarnations
            .iter()
            .any(|c| within(c.crashed_at_ns))
}

/// Whether `machine` left the run exactly at `exit`, inside `[from, to]`:
/// its VM exited then, or one of its incarnations crashed then.
fn left_at(machine: &MachineOutcome, exit: u64, from: u64, to: u64) -> bool {
    (machine.state.exited() && machine.time_ns == exit)
        || machine
            .crashed_incarnations
            .iter()
            .any(|c| c.crashed_at_ns == exit && exit >= from && exit <= to)
}

/// The outcome, witness and fault evidence both engines report, checked
/// against the request and the control transcript.
pub(super) fn checked_report(
    raw: WorldEvidence,
    machines: &[MachineOutcome],
    request: &ExecutionRequest<'_>,
) -> Result<ExecutionReport> {
    let world_witness = ContentDigest::try_from(raw.world_witness.clone())?;
    let properties = raw.properties.clone();
    for machine in machines {
        ContentDigest::try_from(machine.witness.clone())?;
        machine
            .state_digest
            .clone()
            .map(ContentDigest::try_from)
            .transpose()?;
        machine
            .state_digest_parts
            .clone()
            .map(StateDigestParts::try_from)
            .transpose()?;
    }
    let names: BTreeSet<_> = machines.iter().map(|machine| &machine.name).collect();
    if names.len() != machines.len() || names != request.prepared.guests.keys().collect() {
        return Err(Error::Integrity("VMM report machine set mismatch".into()));
    }
    let workload = request
        .prepared
        .placement
        .get(&request.check.service)
        .ok_or_else(|| Error::Integrity("missing workload placement".into()))?;
    let workload_exit = machines
        .iter()
        .find(|machine| &machine.name == workload)
        .and_then(|machine| machine.guest_exit_code)
        .map(|code| WorkloadExit {
            service: request.check.service.clone(),
            code,
        });
    let infrastructure_error = machines
        .iter()
        .find(|machine| machine.guest_init_error.is_some() || machine.infrastructure_error);
    let mut reason = if infrastructure_error.is_some() {
        StopReason::Error
    } else {
        match raw.outcome {
            RunOutcome::EndMachineExited => StopReason::Completed,
            RunOutcome::Duration => StopReason::SimulatedTimeLimit,
            RunOutcome::Quiescent => StopReason::Quiescent,
            RunOutcome::FaultFailed => StopReason::Error,
            other @ (RunOutcome::Checkpoint | RunOutcome::Barrier) => {
                return Err(Error::Integrity(format!("unknown VMM outcome {other}")))
            }
        }
    };
    let full_state = match request.prepared.profile.witness_mode {
        WitnessMode::Exits => None,
        WitnessMode::FullState => Some(
            machines
                .iter()
                .map(|machine| {
                    let digest = machine
                        .state_digest
                        .clone()
                        .ok_or_else(|| Error::Integrity("missing machine state digest".into()))?;
                    Ok((machine.name.clone(), ContentDigest::try_from(digest)?))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        ),
    };
    if request.control.is_none() && (!raw.control_history.is_empty() || raw.control_latency_ns != 0)
    {
        return Err(Error::Integrity("unexpected control transcript".into()));
    }
    let program = external_program(request)?;
    let mut external_plans = validate_triggers(
        raw.triggers.as_deref(),
        &raw.control_history,
        request,
        &program,
        &raw.input_armed,
    )?;
    let scheduled = validate_input_admissions(
        &program,
        &raw.input_admissions,
        &raw.input_rejections,
        &raw.input_branches,
        &raw.input_armed,
        request.limits.simulated_time_ns,
    )?;
    external_plans.extend(scheduled.iter().cloned());
    let mut expected = if request.control.is_some() {
        validate_control_history(
            &raw.control_history,
            raw.control_latency_ns,
            request,
            raw.fault_error.is_some(),
            external_plans,
        )?
    } else {
        external_plans.sort_by_key(|plan| plan.id);
        if external_plans
            .iter()
            .enumerate()
            .any(|(i, plan)| plan.id as usize != i + 1)
        {
            return Err(correlation_mismatch());
        }
        ValidatedControlHistory {
            plans: external_plans,
            ..Default::default()
        }
    };
    expected
        .interventions
        .extend(scheduled.iter().map(|plan| plan.id));

    let mut fault_failure = raw.fault_error.clone();
    if raw.faults.len() != expected.plans.len()
        && !(request.control.is_some() && fault_failure.is_some())
    {
        fault_failure = Some("fault request history is incomplete".to_string());
    }
    let mut faults = Vec::new();
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
        let check = FaultCheck {
            raw: &raw,
            machines,
            expected: &expected,
            request,
            fault,
            plan,
        };
        match check.effect()? {
            FaultEffect::Applied(event) => faults.push(event),
            FaultEffect::Failed(failure) => fault_failure = Some(failure),
            FaultEffect::None => {}
        }
    }
    if fault_failure.is_some() {
        reason = StopReason::Error;
    }
    Ok(ExecutionReport {
        host_wait_ns: 0,
        simulated_time_ns: machines.iter().map(|machine| machine.time_ns).max(),
        reason,
        workload_exit,
        witness: Some(world_witness),
        full_state,
        faults,
        detail: fault_failure.or_else(|| {
            infrastructure_error
                .map(|machine| format!("machine {} failed in guest bootstrap or VMM", machine.name))
        }),
        properties,
    })
}

/// What one fault acknowledgement contributes to the execution report.
enum FaultEffect {
    Applied(crate::execution::FaultEvent),
    Failed(String),
    None,
}

/// Admission IDs break ties, but a later admission can schedule an earlier
/// effect. Report validation must use the same ordering as the engine queue.
fn prior_lifecycle<'a>(plan: &Plan, plans: &'a [Plan], machine: Option<&str>) -> Option<&'a Plan> {
    plans
        .iter()
        .filter(|prior| {
            (prior.at_ns, prior.id) < (plan.at_ns, plan.id)
                && prior.action.machine() == machine
                && matches!(prior.action, Action::Crash { .. } | Action::Reboot { .. })
        })
        .max_by_key(|prior| (prior.at_ns, prior.id))
}

/// One fault acknowledgement, already correlated with its plan.
struct FaultCheck<'a> {
    raw: &'a WorldEvidence,
    machines: &'a [MachineOutcome],
    expected: &'a ValidatedControlHistory,
    request: &'a ExecutionRequest<'a>,
    fault: &'a FaultRecord,
    plan: &'a Plan,
}

impl FaultCheck<'_> {
    fn target_name(&self) -> Option<&str> {
        self.plan.action.machine().or_else(|| {
            let Action::Heal { handle } = &self.plan.action else { return None; };
            self.expected.plans.iter().filter(|prior| {
                prior.id < self.plan.id && prior.at_ns <= self.plan.at_ns
                    && matches!(&prior.action, Action::ClockSkew { handle: owner, .. } | Action::ClockDrift { handle: owner, .. } if owner == handle)
            }).max_by_key(|prior| (prior.at_ns, prior.id)).and_then(|prior| prior.action.machine())
        })
    }

    fn target(&self) -> Option<&MachineOutcome> {
        self.machines
            .iter()
            .find(|m| Some(m.name.as_str()) == self.target_name())
    }

    /// A triggered stop has no RPC reply; an unavailable one needs the same
    /// exit/crash window or prior-down evidence as a process RPC.
    fn triggered_stop_unavailable(&self) -> bool {
        let (fault, plan) = (self.fault, self.plan);
        self.raw
            .triggers
            .as_ref()
            .is_some_and(|records| records.iter().any(|r| r.fault_id == Some(fault.id)))
            && matches!(plan.action, Action::Stop { .. })
            && (self
                .target()
                .is_some_and(|m| left_within(m, plan.at_ns, plan.deadline_ns))
                || self.down_by_prior_crash())
    }

    /// The latest earlier lifecycle plan for the target crashed it, and that
    /// crash applied at its admission.
    fn down_by_prior_crash(&self) -> bool {
        let plan = self.plan;
        prior_lifecycle(plan, &self.expected.plans, self.target_name()).is_some_and(|prior| {
            matches!(prior.action, Action::Crash { .. })
                && prior.at_ns <= plan.at_ns
                && self
                    .raw
                    .faults
                    .get(prior.id as usize - 1)
                    .is_some_and(|fault| {
                        fault.status == RowStatus::Applied
                            && fault.applied_at_ns == Some(prior.at_ns)
                    })
        })
    }

    /// An unavailable process fault needs a prior crash without reboot at
    /// admission, or the target's exact exit/crash time in its window.
    fn process_unavailable(&self) -> bool {
        let (fault, plan) = (self.fault, self.plan);
        if self.expected.interventions.contains(&fault.id) {
            return (plan.action.is_process()
                || matches!(
                    plan.action,
                    Action::ClockSkew { .. } | Action::ClockDrift { .. } | Action::Heal { .. }
                ))
                && (self
                    .target()
                    .is_some_and(|m| left_within(m, plan.at_ns, plan.deadline_ns))
                    || self.down_by_prior_crash());
        }
        self.expected
            .unavailable
            .get(&fault.id)
            .is_some_and(|exit| {
                self.expected.unavailable_while_down.contains(&fault.id)
                    || self
                        .target()
                        .is_some_and(|m| left_at(m, *exit, plan.at_ns, plan.deadline_ns))
            })
    }

    fn effect(&self) -> Result<FaultEffect> {
        let (fault, plan, request) = (self.fault, self.plan, self.request);
        let invalid = || Error::Integrity("invalid fault effect status/time".into());
        let controlled =
            request.control.is_some() || self.expected.interventions.contains(&fault.id);
        match (fault.status, fault.applied_at_ns) {
            (RowStatus::Unavailable, None) if self.triggered_stop_unavailable() => {
                Ok(FaultEffect::None)
            }
            (RowStatus::Unavailable, None) if self.process_unavailable() => Ok(FaultEffect::None),
            _ if self.expected.unavailable.contains_key(&fault.id) => Err(invalid()),
            (RowStatus::Applied, Some(at))
                if at >= fault.requested_at_ns
                    && at <= plan.deadline_ns
                    && (!controlled || !plan.action.is_lifecycle() || at == plan.at_ns)
                    && (!controlled
                        || self
                            .expected
                            .effects
                            .get(&fault.id)
                            .is_none_or(|e| *e == at)) =>
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
                Ok(FaultEffect::Applied(crate::execution::FaultEvent {
                    requested_at_ns: fault.requested_at_ns,
                    applied_at_ns: at,
                    service,
                    action: fault.action.clone(),
                    detail: format!("id={} {}", fault.id, fault.detail),
                }))
            }
            (RowStatus::Requested | RowStatus::Unrequested, None)
                if !self.expected.effects.contains_key(&fault.id) =>
            {
                Ok(FaultEffect::None)
            }
            (RowStatus::Failed, None) => Ok(FaultEffect::Failed(format!(
                "fault {} failed: {}",
                fault.id, fault.detail
            ))),
            _ => Err(invalid()),
        }
    }
}

#[cfg(test)]
mod typed_trigger_tests {
    use super::*;
    use seedbox_engine_protocol::input_program::{ArmReceipt, TimedInput};

    #[test]
    fn typed_trigger_records_must_match_their_input_and_receipt() {
        let program: Vec<TimedInput> = vec![serde_json::from_value(serde_json::json!({
            "key": "kill",
            "at_ns": 5,
            "action": {
                "action": "trigger",
                "when": {"on": "event", "name": "commit"},
                "nth": 2,
                "within_ns": 100,
                "then": {"action": "crash", "machine": "db"},
            },
        }))
        .unwrap()];
        let mut triggers = seedbox_engine_core::triggers::Triggers::default();
        let seedbox_engine_protocol::input_program::InputAction::Rule(
            seedbox_engine_protocol::input_program::RuleAction::Trigger(spec),
        ) = &program[0].action
        else {
            panic!("not a trigger");
        };
        triggers.arm_typed("kill", spec, 5).unwrap();
        let record = triggers.records[0].clone();
        let receipt = ArmReceipt {
            key: "kill".into(),
            at_ns: 5,
            trigger_id: 1,
        };
        correlate_typed_triggers(&[&record], &program, std::slice::from_ref(&receipt)).unwrap();
        assert!(correlate_typed_triggers(&[&record], &program, &[]).is_err());
        let mut tampered = [record.clone(), record.clone(), record.clone()];
        tampered[0].nth = 3;
        tampered[1].match_deadline_ns = u64::MAX;
        tampered[2].key = Some("other".into());
        for bad in &tampered {
            let receipts = std::slice::from_ref(&receipt);
            assert!(correlate_typed_triggers(&[bad], &program, receipts).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn input_admissions_are_an_exact_prefix_with_runtime_ids() {
        use super::*;
        use seedbox_engine_protocol::input_program::{InputAdmission, TimedInput};
        let program: Vec<_> = (0..3)
            .map(|i| TimedInput {
                timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
                key: format!("input-{i}"),
                at_ns: 10 + i,
                action: Action::Stop {
                    machine: "a".into(),
                }
                .into(),
            })
            .collect();
        let admissions: Vec<_> = program
            .iter()
            .take(2)
            .enumerate()
            .map(|(i, input)| InputAdmission {
                key: input.key.clone(),
                plan: Plan {
                    id: 2 + i as u32 * 3,
                    at_ns: input.at_ns,
                    deadline_ns: input.at_ns + PROCESS_EFFECT_TIMEOUT_NS,
                    action: input.action.fault().expect("a fault input").clone(),
                },
            })
            .collect();
        assert_eq!(
            validate_input_admissions(&program, &admissions, &[], &[], &[], 100)
                .unwrap()
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>(),
            [2, 5]
        );
        assert!(validate_input_admissions(&program, &[], &[], &[], &[], 100)
            .unwrap()
            .is_empty());
        for field in 0..5 {
            let mut bad = admissions.clone();
            match field {
                0 => bad[0].key = "other".into(),
                1 => bad[0].plan.at_ns += 1,
                2 => bad[0].plan.deadline_ns += 1,
                3 => bad[1].plan.id = bad[0].plan.id,
                _ => {
                    bad[0].plan.action = Action::Crash {
                        machine: "a".into(),
                    }
                }
            }
            assert!(validate_input_admissions(&program, &bad, &[], &[], &[], 100).is_err());
        }
    }
    use super::*;

    #[test]
    fn rejected_override_is_a_consumed_prefix_entry_without_reserving_a_fault_id() {
        use seedbox_engine_protocol::input_program::{InputAdmission, InputRejection, TimedInput};
        let input = |key: &str| TimedInput {
            key: key.into(),
            at_ns: 10,
            timeout_ns: 37,
            action: seedbox_control::Action::Heal {
                handle: "cut".into(),
            }
            .into(),
        };
        let program = [input("missing"), input("valid")];
        let admissions = [InputAdmission {
            key: "valid".into(),
            plan: seedbox_control::Plan {
                id: 1,
                at_ns: 10,
                deadline_ns: 47,
                action: program[1].action.fault().expect("a fault input").clone(),
            },
        }];
        let mut rejections = [InputRejection {
            key: "missing".into(),
            at_ns: 10,
            reason: "heal of inactive handle".into(),
        }];
        assert_eq!(
            super::validate_input_admissions(&program, &admissions, &rejections, &[], &[], 100)
                .unwrap()[0]
                .id,
            1
        );
        rejections[0].at_ns += 1;
        assert!(super::validate_input_admissions(
            &program,
            &admissions,
            &rejections,
            &[],
            &[],
            100
        )
        .is_err());
        assert!(
            super::validate_input_admissions(&program, &admissions, &[], &[], &[], 100).is_err()
        );
    }

    #[test]
    fn prior_lifecycle_uses_effect_time_then_admission_order() {
        let plan = |id, at_ns, action| Plan {
            id,
            at_ns,
            deadline_ns: at_ns,
            action,
        };
        let target = plan(
            2,
            20,
            Action::Stop {
                machine: "a".into(),
            },
        );
        let mut history = vec![
            plan(
                1,
                100,
                Action::Reboot {
                    machine: "a".into(),
                },
            ),
            target.clone(),
            plan(
                3,
                10,
                Action::Crash {
                    machine: "a".into(),
                },
            ),
            plan(
                4,
                20,
                Action::Reboot {
                    machine: "a".into(),
                },
            ),
        ];
        assert_eq!(
            prior_lifecycle(&target, &history, target.action.machine())
                .unwrap()
                .id,
            3
        );
        history[0].at_ns = 20;
        assert_eq!(
            prior_lifecycle(&target, &history, target.action.machine())
                .unwrap()
                .id,
            1
        );
        history[0].action = Action::Reboot {
            machine: "b".into(),
        };
        assert_eq!(
            prior_lifecycle(&target, &history, target.action.machine())
                .unwrap()
                .id,
            3
        );
    }
}
