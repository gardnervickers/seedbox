//! Language-neutral runtime control. Fixed latency is semantic, never selected
//! from host performance or another machine's observed progress.
use crate::world_config::ControlConfig;
use seedbox_control::{Action, Header, Operation, Plan, Request, Target};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn partial_transport_and_pending_call_continue_after_serialization() {
        let mut framer = ManagementFramer::default();
        assert!(framer
            .receive(b"SBX_RPC1 1 1 {\"version\":1,")
            .unwrap()
            .is_empty());
        let mut framer: ManagementFramer =
            serde_json::from_slice(&serde_json::to_vec(&framer).unwrap()).unwrap();
        let frames = framer.receive(b"\"id\":1,\"op\":\"now\"}\n").unwrap();
        assert_eq!(frames.len(), 1);
        let names = vec!["scenario".into()];
        let mut original = Control::new(
            &json!({"machine":"scenario","latency_ns":20,"placement":{"scenario":"scenario"}}),
            names.clone(),
            10,
            5,
        )
        .unwrap();
        original.request(0, 100, &frames[0], 115).unwrap();
        let mut restored: Control =
            serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
        let mut replies = Vec::new();
        for control in [&mut original, &mut restored] {
            let mut faults = crate::faults::Faults::new(vec![], names.clone()).unwrap();
            control.apply_boundary(&mut faults, 120).unwrap();
            replies.push(control.response(140).unwrap());
            control.mark_delivered(140).unwrap();
        }
        assert_eq!(replies[0], replies[1]);
        assert_eq!(
            serde_json::to_value(original).unwrap(),
            serde_json::to_value(restored).unwrap()
        );
        for invalid in [
            vec![b'x'; MAX_TRANSPORT_FRAME + 1],
            b"already complete\n".to_vec(),
        ] {
            assert!(serde_json::from_value::<ManagementFramer>(
                serde_json::to_value(invalid).unwrap()
            )
            .is_err());
        }
    }
}

/// Admission policy shared by guest controls and host interventions. A dead VM
/// can still be crashed and rebooted; process and clock controls need a live VM.
pub(crate) fn exited_target_error(action: &Action) -> Option<&'static str> {
    match action {
        Action::Pause { .. } | Action::Resume { .. } => {
            Some("target VM has exited; crash and reboot it instead")
        }
        Action::ClockSkew { .. } | Action::ClockDrift { .. } => Some("target VM has exited"),
        Action::PauseProcess { .. } | Action::ResumeProcess { .. } => {
            Some("target VM has exited; there is no process to pause or resume")
        }
        Action::Stop { .. } | Action::Restart { .. } => {
            Some("target VM has exited; restart only starts a stopped process")
        }
        _ => None,
    }
}

pub const MAX_FRAME: usize = 16384;
pub const MAX_TRANSPORT_FRAME: usize = MAX_FRAME + 64;
/// Frame actual transport bytes; adapters exclude register-only writes.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Vec<u8>", into = "Vec<u8>")]
pub struct ManagementFramer {
    line: Vec<u8>,
}
impl From<ManagementFramer> for Vec<u8> {
    fn from(framer: ManagementFramer) -> Self {
        framer.line
    }
}
impl TryFrom<Vec<u8>> for ManagementFramer {
    type Error = String;
    fn try_from(line: Vec<u8>) -> Result<Self, Self::Error> {
        if line.len() > MAX_TRANSPORT_FRAME || line.contains(&b'\n') {
            return Err("invalid checkpoint management frame".into());
        }
        Ok(Self { line })
    }
}
impl ManagementFramer {
    /// Accept a byte only after its transport actually transmits it.
    pub fn transmitted_byte(&mut self, byte: u8) -> Result<Option<Vec<u8>>, String> {
        if byte == b'\n' {
            let line = std::mem::take(&mut self.line);
            return Ok(line.starts_with(b"SBX_RPC1 ").then_some(line));
        }
        if self.line.len() >= MAX_TRANSPORT_FRAME {
            return Err("management frame too large".into());
        }
        self.line.push(byte);
        Ok(None)
    }
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut frames = Vec::new();
        for byte in bytes {
            if let Some(frame) = self.transmitted_byte(*byte)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub relay: u32,
    pub session: u32,
    pub request: Value,
    pub requested: u64,
    pub admitted: u64,
    pub admitted_done: bool,
    pub effect: Option<u64>,
    pub reply: Option<u64>,
    pub fault: Option<u32>,
    pub plan: Option<Plan>,
    pub operation: Option<Operation>,
    pub result: Value,
    pub error: Option<String>,
    pub error_code: Option<std::borrow::Cow<'static, str>>,
    pub fatal: bool,
    /// A planned `arm_trigger`, armed at admission.
    pub trigger: Option<TriggerArm>,
}
/// An `arm_trigger` request resolved to its source machine and fault action.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerArm {
    pub source: String,
    pub marker: Vec<u8>,
    pub timeout_ns: u64,
    pub action: Action,
}
impl TriggerArm {
    /// The planned trigger as a cut-off call reports it.
    fn evidence(&self) -> Value {
        json!({
            "trigger_source": self.source,
            "trigger_marker": self.marker,
            "trigger_timeout_ns": self.timeout_ns,
            "trigger_action": self.action,
        })
    }
}
/// What planning an operation decided.
struct Planned {
    plan: Option<Plan>,
    result: Value,
    effect: u64,
    trigger: Option<TriggerArm>,
}
/// Resolve both sides of a partition or shape in order, refusing any machine
/// that appears twice on either side.
pub(crate) fn disjoint_sides<T, N: Ord + Clone>(
    sides: [&[T]; 2],
    mut resolve: impl FnMut(&T) -> Result<N, String>,
    overlap: &str,
) -> Result<[Vec<N>; 2], String> {
    let mut members = BTreeSet::new();
    let mut resolved = [Vec::new(), Vec::new()];
    for (side, out) in sides.into_iter().zip(&mut resolved) {
        for item in side {
            let n = resolve(item)?;
            if !members.insert(n.clone()) {
                return Err(overlap.into());
            }
            out.push(n);
        }
    }
    Ok(resolved)
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub controller: Option<usize>,
    pub latency: u64,
    pub pending: Option<Call>,
    pub log: Vec<Value>,
    pub error: Option<String>,
    pub triggers: crate::triggers::Triggers,
    names: Vec<String>,
    placement: BTreeMap<String, String>,
    last_relay: u32,
    last_session: u32,
    last_public: u32,
    handles: u32,
    lookahead: u64,
    undelivered_reply: bool,
}
impl Control {
    /// Build from a world document's `control` object held as JSON; `null`
    /// has no controller.
    pub fn new(
        config: &Value,
        names: Vec<String>,
        lookahead: u64,
        early: u64,
    ) -> Result<Self, String> {
        let config =
            Option::<ControlConfig>::deserialize(config).map_err(|e| format!("control: {e}"))?;
        Self::from_config(config.as_ref(), names, lookahead, early)
    }
    pub fn from_config(
        config: Option<&ControlConfig>,
        names: Vec<String>,
        lookahead: u64,
        early: u64,
    ) -> Result<Self, String> {
        let controller = config
            .map(|c| {
                names
                    .iter()
                    .position(|n| *n == c.machine)
                    .ok_or("unknown control machine")
            })
            .transpose()?;
        let latency = config.map_or(0, |c| c.latency_ns);
        if controller.is_some()
            && latency
                < lookahead
                    .checked_add(early)
                    .ok_or("control latency overflow")?
        {
            return Err("control latency below causal lookahead".into());
        }
        let placement = config.map(|c| c.placement.clone()).unwrap_or_default();
        Ok(Self {
            controller,
            latency,
            pending: None,
            log: vec![],
            error: None,
            triggers: crate::triggers::Triggers::default(),
            names,
            placement,
            last_relay: 0,
            last_session: 0,
            last_public: 0,
            handles: 0,
            lookahead,
            undelivered_reply: false,
        })
    }
    fn target(&self, target: &Target) -> Result<String, String> {
        let name = match target {
            Target::Service(service) => self
                .placement
                .get(service)
                .cloned()
                .ok_or("unknown service")?,
            Target::Machine(machine) => machine.clone(),
        };
        if !self.names.contains(&name) {
            return Err("unknown machine".into());
        }
        if Some(name.as_str()) == self.controller.map(|i| self.names[i].as_str()) {
            return Err("controller is outside failure model".into());
        }
        Ok(name)
    }
    /// Allocate the next public fault handle and report it in `result`.
    fn next_handle(&mut self, exhausted: &str, result: &mut Value) -> Result<String, String> {
        self.handles = self.handles.checked_add(1).ok_or(exhausted)?;
        let handle = format!("p{}", self.handles);
        *result = json!({ "handle": handle });
        Ok(handle)
    }
    fn plan(&mut self, op: &Operation, at: u64) -> Result<Planned, String> {
        let mut result = json!({});
        let mut effect = at;
        let mut trigger = None;
        let action = match op {
            Operation::Stop { target } => Some(Action::Stop {
                machine: self.target(target)?,
            }),
            Operation::Restart { target } => Some(Action::Restart {
                machine: self.target(target)?,
            }),
            Operation::PauseProcess { target } => Some(Action::PauseProcess {
                machine: self.target(target)?,
            }),
            Operation::ResumeProcess { target } => Some(Action::ResumeProcess {
                machine: self.target(target)?,
            }),
            Operation::Pause { target } => Some(Action::Pause {
                machine: self.target(target)?,
            }),
            Operation::Resume { target } => Some(Action::Resume {
                machine: self.target(target)?,
            }),
            Operation::Crash { target } => Some(Action::Crash {
                machine: self.target(target)?,
            }),
            Operation::Reboot { target } => Some(Action::Reboot {
                machine: self.target(target)?,
            }),
            Operation::Partition { left, right } => {
                if left.is_empty() || right.is_empty() {
                    return Err("partition sides must be nonempty".into());
                }
                let [left, right] = disjoint_sides(
                    [left, right],
                    |t| self.target(t),
                    "partition sides must be disjoint with unique machines",
                )?;
                let handle = self.next_handle("partition handles exhausted", &mut result)?;
                Some(Action::Partition {
                    handle,
                    left,
                    right,
                })
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
                crate::links::Policy {
                    latency_ns: *latency_ns,
                    jitter_ns: *jitter_ns,
                    loss_ppm: *loss_ppm,
                    bandwidth_bps: *bandwidth_bps,
                    queue_bound_ns: *queue_bound_ns,
                }
                .validate_shape(self.lookahead, network_faults)?;
                if from.is_empty() || to.is_empty() {
                    return Err("shape sides must be nonempty".into());
                }
                let [from, to] = disjoint_sides(
                    [from, to],
                    |t| self.target(t),
                    "shape sides must be disjoint with unique machines",
                )?;
                let handle = self.next_handle("fault handles exhausted", &mut result)?;
                Some(Action::Shape {
                    handle,
                    from,
                    to,
                    latency_ns: *latency_ns,
                    jitter_ns: *jitter_ns,
                    loss_ppm: *loss_ppm,
                    bandwidth_bps: *bandwidth_bps,
                    queue_bound_ns: *queue_bound_ns,
                    network_faults: *network_faults,
                })
            }
            Operation::Connections { from, to, .. } => {
                if self.target(from)? == self.target(to)? {
                    return Err("connection endpoints must differ".into());
                }
                None
            }
            Operation::DropConnection { connection } => {
                let from = self.target(&Target::Machine(connection.from.clone()))?;
                let to = self.target(&Target::Machine(connection.to.clone()))?;
                if from == to
                    || connection.incarnation == 0
                    || connection.src_port == 0
                    || connection.dst_port == 0
                {
                    return Err("invalid connection selector".into());
                }
                let handle = self.next_handle("fault handles exhausted", &mut result)?;
                Some(Action::DropConnection {
                    handle,
                    connection: connection.clone(),
                })
            }
            Operation::ClockSkew {
                target,
                offset_ns,
                max_offset_ns,
            } => {
                seedbox_control::validate_clock_skew(*offset_ns, *max_offset_ns)?;
                let machine = self.target(target)?;
                let handle = self.next_handle("fault handles exhausted", &mut result)?;
                Some(Action::ClockSkew {
                    handle,
                    machine,
                    offset_ns: *offset_ns,
                    max_offset_ns: *max_offset_ns,
                })
            }
            Operation::ClockDrift {
                target,
                rate_ppb,
                max_rate_ppb,
            } => {
                seedbox_control::validate_clock_drift(*rate_ppb, *max_rate_ppb)?;
                let machine = self.target(target)?;
                self.handles = self
                    .handles
                    .checked_add(1)
                    .ok_or("fault handles exhausted")?;
                let handle = format!("p{}", self.handles);
                result = json!({"handle":handle});
                Some(Action::ClockDrift {
                    handle,
                    machine,
                    rate_ppb: *rate_ppb,
                    max_rate_ppb: *max_rate_ppb,
                })
            }
            Operation::Throttle { target, speed_hz } => {
                let range = seedbox_control::THROTTLE_SPEED_HZ;
                if !range.contains(speed_hz) {
                    return Err(format!(
                        "speed_hz must be in [{}, {}]",
                        range.start(),
                        range.end()
                    ));
                }
                let machine = self.target(target)?;
                let handle = self.next_handle("fault handles exhausted", &mut result)?;
                Some(Action::Throttle {
                    handle,
                    machine,
                    speed_hz: *speed_hz,
                })
            }
            Operation::Heal { handle } => {
                if handle.is_empty() {
                    return Err("handle must be nonempty string".into());
                }
                Some(Action::Heal {
                    handle: handle.clone(),
                })
            }
            Operation::Sleep { duration_ns } => {
                effect = at.checked_add(*duration_ns).ok_or("sleep overflow")?;
                None
            }
            Operation::Now {} => {
                result = json!({ "now_ns": at });
                None
            }
            Operation::Inspect {} => None,
            Operation::ArmTrigger {
                source,
                marker,
                match_timeout_ns,
                fault,
            } => {
                let arm = self.plan_trigger(source, marker, *match_timeout_ns, fault, at)?;
                // Until admission arms it, the call's evidence is the plan.
                result = arm.evidence();
                trigger = Some(arm);
                None
            }
            Operation::CancelTrigger { .. } => None,
        };
        let plan = action
            .map(|action| {
                Ok::<_, String>(Plan {
                    id: 0,
                    at_ns: at,
                    deadline_ns: at
                        .checked_add(seedbox_control::PROCESS_EFFECT_TIMEOUT_NS)
                        .ok_or("fault deadline overflow")?,
                    action,
                })
            })
            .transpose()?;
        Ok(Planned {
            plan,
            result,
            effect,
            trigger,
        })
    }
    fn plan_trigger(
        &mut self,
        source: &Target,
        marker: &[u8],
        match_timeout_ns: u64,
        fault: &seedbox_control::TriggerFault,
        at: u64,
    ) -> Result<TriggerArm, String> {
        if !seedbox_control::valid_trigger_marker(marker) || match_timeout_ns == 0 {
            return Err(format!(
                "trigger requires 1..={} marker bytes and positive match timeout",
                seedbox_control::MAX_TRIGGER_MARKER
            ));
        }
        at.checked_add(match_timeout_ns)
            .and_then(|d| d.checked_add(self.latency))
            .and_then(|d| d.checked_add(seedbox_control::PROCESS_EFFECT_TIMEOUT_NS))
            .ok_or("trigger deadline overflow")?;
        seedbox_control::check_trigger_budget(self.triggers.records.len())?;
        let source = match source {
            Target::Service(s) => self
                .placement
                .get(s)
                .cloned()
                .ok_or("unknown trigger source service")?,
            Target::Machine(m) => m.clone(),
        };
        if !self.names.contains(&source) {
            return Err("unknown trigger source machine".into());
        }
        let action = self
            .plan(&fault.operation(), at)?
            .plan
            .ok_or("missing trigger action")?
            .action;
        Ok(TriggerArm {
            source,
            marker: marker.to_vec(),
            timeout_ns: match_timeout_ns,
            action,
        })
    }
    pub fn request(
        &mut self,
        machine: usize,
        time: u64,
        line: &[u8],
        max_time: u64,
    ) -> Result<(), String> {
        self.request_with_support(machine, time, line, max_time, |_| true)
    }
    /// Reject unsupported operations before allocating handles or planning effects.
    pub fn request_with_support(
        &mut self,
        machine: usize,
        time: u64,
        line: &[u8],
        max_time: u64,
        supports: impl Fn(&Operation) -> bool,
    ) -> Result<(), String> {
        if self.controller != Some(machine) {
            return Err("control request from unauthorized machine".into());
        }
        if self.pending.is_some() || self.undelivered_reply {
            return Err("control request while another is outstanding".into());
        }
        if line.len() > MAX_TRANSPORT_FRAME {
            return Err("control frame too large".into());
        }
        let text = std::str::from_utf8(line).map_err(|_| "control frame UTF-8")?;
        let tail = text.strip_prefix("SBX_RPC1 ").ok_or("control prefix")?;
        let (relay, tail) = tail.split_once(' ').ok_or("control frame envelope")?;
        let (session, payload) = tail.split_once(' ').ok_or("control session envelope")?;
        let session = session.parse::<u32>().map_err(|_| "control session ID")?;
        if session == 0 || session < self.last_session {
            return Err("stale control session ID".into());
        }
        if payload.len() + 1 > MAX_FRAME {
            return Err("public control JSON frame too large".into());
        }
        let relay = relay.parse::<u32>().map_err(|_| "control relay ID")?;
        if relay == 0 || relay != self.last_relay.checked_add(1).ok_or("relay ID exhausted")? {
            return Err("stale/nonsequential control relay ID".into());
        }
        let header: Header =
            serde_json::from_str(payload).map_err(|_| "malformed control identity/JSON")?;
        if header.version != 1 {
            return Err("control version/id".into());
        }
        let public = header.id.get();
        let expected = if session > self.last_session {
            Some(1)
        } else {
            self.last_public.checked_add(1)
        };
        if Some(public) != expected {
            return Err("stale/nonsequential public control ID".into());
        }
        let admitted = time
            .checked_add(self.latency)
            .ok_or("control admission overflow")?;
        if max_time > admitted {
            return Err("control admission is in another machine's past".into());
        }
        self.last_session = session;
        self.last_public = public;
        self.last_relay = relay;
        let request: Result<Request, _> = serde_json::from_str(payload);
        let mut unsupported = false;
        let refused = |error| {
            let planned = Planned {
                plan: None,
                result: json!({}),
                effect: admitted,
                trigger: None,
            };
            (planned, Some(error))
        };
        let (operation, (planned, error)) = match request {
            Ok(request) => {
                let operation = request.operation;
                let outcome = if !supports(&operation) {
                    unsupported = true;
                    refused("operation is not supported by this backend".into())
                } else {
                    match self.plan(&operation, admitted) {
                        Ok(planned) => (planned, None),
                        Err(error) => refused(error),
                    }
                };
                (Some(operation), outcome)
            }
            Err(error) => (None, refused(format!("invalid request arguments: {error}"))),
        };
        let Planned {
            plan,
            result,
            effect,
            trigger,
        } = planned;
        let r: Value = serde_json::from_str(payload).map_err(|_| "malformed control JSON")?;
        self.pending = Some(Call {
            relay,
            session,
            request: r,
            requested: time,
            admitted,
            admitted_done: false,
            effect: plan.is_none().then_some(effect),
            reply: None,
            fault: None,
            plan,
            operation,
            result,
            error,
            error_code: unsupported.then_some("unsupported".into()),
            fatal: false,
            trigger,
        });
        Ok(())
    }
    /// Validate target VM existence at the admission barrier, never at request observation.
    pub fn reject_unavailable(&mut self, exited: &[String]) {
        if let Some(p) = self.pending.as_mut() {
            let rejection = p.plan.as_ref().and_then(|plan| {
                plan.action
                    .machine()
                    .filter(|name| exited.iter().any(|n| n == name))
                    .and_then(|_| exited_target_error(&plan.action))
            });
            if let Some(message) = rejection.filter(|_| !p.admitted_done) {
                p.plan = None;
                p.effect = Some(p.admitted);
                p.error = Some(message.into());
                p.error_code = Some("target_unavailable".into());
            }
        }
    }
    /// Throttle only machines whose timebase charges CPU work by a speed.
    pub fn reject_unthrottleable(&mut self, unsupported: &[String]) {
        if let Some(p) = self.pending.as_mut() {
            if !p.admitted_done
                && p.plan.as_ref().is_some_and(|plan| {
                    matches!(&plan.action, Action::Throttle { machine, .. } if unsupported.contains(machine))
                })
            {
                p.plan = None;
                p.effect = Some(p.admitted);
                p.result = json!({});
                p.error = Some("throttle needs a backend with configurable CPU cost".into());
                p.error_code = Some("unsupported".into());
            }
        }
    }
    pub fn boundary(&self) -> u64 {
        self.pending
            .as_ref()
            .map(|p| {
                if !p.admitted_done {
                    p.admitted
                } else {
                    p.reply.or(p.effect).unwrap_or(u64::MAX)
                }
            })
            .unwrap_or(u64::MAX)
    }
    /// Exact globally committed barrier. This is also exercised without KVM.
    pub fn apply_boundary(
        &mut self,
        faults: &mut crate::faults::Faults,
        now: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        if self.boundary() != now {
            return Err("wrong control barrier".into());
        }
        if self.pending.as_ref().is_some_and(|p| p.reply == Some(now)) {
            return self.response(now).map(Some);
        }
        let discovery = match self.pending.as_ref().and_then(|p| p.operation.as_ref()) {
            Some(Operation::Connections { from, to, after })
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|p| !p.admitted_done && p.error.is_none()) =>
            {
                Some((self.target(from)?, self.target(to)?, *after))
            }
            _ => None,
        };
        let p = self.pending.as_mut().ok_or("no pending control barrier")?;
        if let Some((from, to, after)) = discovery {
            let connections = faults.connections(&from, &to, after);
            let next_after = connections.last().map(|c| c.incarnation).unwrap_or(after);
            p.result = json!({ "connections": connections, "next_after": next_after });
        }
        if !p.admitted_done {
            p.admitted_done = true;
            let trigger_result = match &p.operation {
                Some(Operation::ArmTrigger { .. }) if p.error.is_none() => {
                    let TriggerArm {
                        source,
                        marker,
                        timeout_ns,
                        action,
                    } = p.trigger.take().ok_or("trigger action")?;
                    let handle = match &action {
                        Action::Partition { handle, .. } => Some(handle.clone()),
                        _ => None,
                    };
                    Some(
                        self.triggers
                            .arm(source, marker, now, timeout_ns, action)
                            .map(|id| {
                                let mut result = json!({ "trigger_id": id });
                                if let Some(handle) = handle {
                                    result["handle"] = json!(handle);
                                }
                                result
                            }),
                    )
                }
                Some(Operation::CancelTrigger { trigger_id }) if p.error.is_none() => {
                    Some(self.triggers.cancel(*trigger_id, now).map(|_| json!({})))
                }
                _ => None,
            };
            if let Some(result) = trigger_result {
                match result {
                    Ok(result) => p.result = result,
                    Err(error) => {
                        p.result = json!({});
                        p.error = Some(error);
                        p.error_code = Some("invalid_state".into());
                    }
                }
            }
            if let Some(plan) = p.plan.take() {
                if let Err(error) = faults.validate_admission_state(&plan.action) {
                    p.error = Some(error.message().into());
                    if let Some(code) = error.code() {
                        p.error_code = Some(code.into());
                    }
                    p.effect = Some(now);
                } else {
                    if let Action::Heal { handle } = &plan.action {
                        if let Some(s) = faults.connections.selections.get(handle) {
                            p.result = json!({ "matched": s.matched, "dropped": s.dropped });
                        }
                    }
                    p.fault = Some(faults.admit(plan, now)?);
                }
            }
        }
        if let Some(effect) = p.effect.filter(|&e| e <= now) {
            p.reply = Some(
                effect
                    .checked_add(self.latency)
                    .ok_or("control reply overflow")?,
            );
        }
        Ok(None)
    }
    pub fn observe_faults(
        &mut self,
        faults: &crate::faults::Faults,
        time: u64,
    ) -> Result<(), String> {
        if let Some(p) = self.pending.as_mut() {
            if p.effect.is_none() {
                if let Some(id) = p.fault {
                    let at = match faults.outcome(id) {
                        Some((crate::faults::Status::Applied(at), _)) => Some(at),
                        Some((crate::faults::Status::Failed, detail)) => {
                            p.error = Some(detail.to_owned());
                            p.fatal = true;
                            Some(time)
                        }
                        Some((crate::faults::Status::Unavailable(at), detail)) => {
                            p.error = Some(detail.to_owned());
                            p.error_code = Some("target_unavailable".into());
                            Some(at)
                        }
                        _ => None,
                    };
                    if let (Some(at), Some(row)) = (at, faults.row(id)) {
                        if matches!(p.operation, Some(Operation::ClockSkew { .. })) {
                            if let Some(offset) = row.offset_ns {
                                p.result["offset_ns"] = json!(offset);
                            }
                        }
                        if matches!(p.operation, Some(Operation::ClockDrift { .. })) {
                            if let Some(rate) = row.rate_ppb {
                                p.result["rate_ppb"] = json!(rate);
                            }
                        }
                        if matches!(p.operation, Some(Operation::Heal { .. })) {
                            if let Some(counts) = &row.queue_counts {
                                p.result["queue_counts"] = json!(counts);
                            }
                            if let Some(counts) = &row.network_fault_counts {
                                p.result["network_fault_counts"] = json!(counts);
                            }
                        }
                        p.effect = Some(at);
                        p.reply = Some(
                            at.checked_add(self.latency)
                                .ok_or("control reply overflow")?,
                        );
                    }
                }
            }
        }
        Ok(())
    }
    pub fn cut_off(&mut self, faults: &crate::faults::Faults, time: u64) {
        if let Some(p) = self.pending.take() {
            let plan = p.fault.and_then(|id| faults.plan(id)).cloned().or(p.plan);
            let mut row = json!({
                "relay_id": p.relay,
                "session_id": p.session,
                "request": p.request,
                "status": "cut_off",
                "requested_at_ns": p.requested,
                "admitted_at_ns": p.admitted,
                "admitted_done": p.admitted_done,
                "effect_at_ns": p.effect,
                "replied_at_ns": p.reply,
                "cut_off_at_ns": time,
                "fault_id": p.fault,
                "plan": plan,
                "error": p.error,
                "fatal": p.fatal,
            });
            if matches!(
                p.operation,
                Some(Operation::ArmTrigger { .. } | Operation::CancelTrigger { .. })
            ) {
                row["result"] = p.result;
            }
            self.log.push(row);
        }
        if let Some(row) = self.log.last_mut() {
            if row["response"].is_object() && row["delivered_at_ns"].is_null() {
                row["status"] = json!("cut_off");
                row["cut_off_at_ns"] = json!(time);
            }
        }
    }
    pub fn mark_delivered(&mut self, time: u64) -> Result<(), String> {
        let row = self
            .log
            .last_mut()
            .ok_or("unsolicited control reply delivery")?;
        if !self.undelivered_reply || row["response"]["replied_at_ns"].as_u64() != Some(time) {
            return Err("stale/late control reply delivery".into());
        }
        row["delivered_at_ns"] = json!(time);
        self.undelivered_reply = false;
        Ok(())
    }
    pub fn has_undelivered_reply(&self) -> bool {
        self.undelivered_reply
    }
    /// A controller frame that could not be opened as a request: log it and
    /// fail the run.
    pub fn reject_request(&mut self, machine: &str, at: u64, line: &[u8], error: String) {
        self.log.push(json!({
            "status": "rejected",
            "machine": machine,
            "at_ns": at,
            "frame": String::from_utf8_lossy(line),
            "error": error,
        }));
        self.error = Some(error);
    }
    /// A pending `inspect` that its admission barrier has not answered yet.
    pub fn wants_inspect(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| !p.admitted_done && matches!(p.operation, Some(Operation::Inspect {})))
    }
    /// Answer a pending `inspect` with the world as it stands: `services` from
    /// the control placement, each machine's state, and
    /// nominal network counters.
    pub fn answer_inspect(
        &mut self,
        machines: Vec<Value>,
        faults: &crate::faults::Faults,
        network_counters: Value,
    ) -> Result<(), String> {
        let mut result = json!({
            "services": self.placement,
            "machines": machines,
            "active_handles": faults.active_handles(),
            "network_counters": network_counters,
        });
        if let Some(evidence) = self.triggers.evidence()? {
            result["triggers"] = evidence;
        }
        if let Some(p) = self.pending.as_mut() {
            p.result = result;
        }
        Ok(())
    }
    pub fn response(&mut self, time: u64) -> Result<Vec<u8>, String> {
        let Some(mut p) = self.pending.take_if(|p| p.reply == Some(time)) else {
            return Err("RPC response at wrong virtual time".into());
        };
        let mut reply = json!({
            "version": 1,
            "id": p.request["id"],
            "ok": p.error.is_none(),
            "result": p.result,
            "requested_at_ns": p.requested,
            "admitted_at_ns": p.admitted,
            "effect_at_ns": p.effect,
            "replied_at_ns": time,
        });
        if p.error.is_none()
            && serde_json::to_vec(&reply).map_err(|e| e.to_string())?.len() + 1 > MAX_FRAME
        {
            p.error = Some("operation result exceeds response frame bound".into());
            p.error_code = Some("result_too_large".into());
        }
        if let Some(error) = p.error {
            let handle = reply["result"].get("handle").cloned();
            if let Value::Object(fields) = &mut reply {
                fields.remove("result");
            }
            let code = if p.fatal {
                "effect_failed"
            } else {
                p.error_code.as_deref().unwrap_or("invalid_request")
            };
            reply["ok"] = json!(false);
            reply["error"] = json!({ "code": code, "message": error });
            // An unavailable effect still allocated its handle; keep it so the
            // planned action can be reconstructed from the reply.
            if let (Some(handle), Some("target_unavailable")) = (handle, p.error_code.as_deref()) {
                reply["error"]["handle"] = handle;
            }
            if p.fatal {
                self.error = Some(error);
            }
        }
        if serde_json::to_vec(&reply).map_err(|e| e.to_string())?.len() + 1 > MAX_FRAME {
            return Err("control error response exceeds frame bound".into());
        }
        self.undelivered_reply = true;
        self.log.push(json!({
            "relay_id": p.relay,
            "session_id": p.session,
            "request": p.request,
            "response": reply,
            "fault_id": p.fault,
        }));
        Ok(format!("SBX_REPLY1 {} {} {}\n", p.relay, p.session, reply).into_bytes())
    }
}
