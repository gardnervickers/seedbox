//! Language-neutral runtime control. Fixed latency is semantic, never selected
//! from host performance or another machine's observed progress.
use seedbox_control::{Action, Header, Operation, Plan, Request, Target};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_FRAME: usize = 16384;
pub const MAX_TRANSPORT_FRAME: usize = MAX_FRAME + 64;
/// Frame only actual UART transmissions, excluding divisor-register writes.
#[derive(Default, Clone)]
pub struct ManagementFramer {
    line: Vec<u8>,
}
impl ManagementFramer {
    pub fn write(
        &mut self,
        uart: &mut crate::devices::Uart,
        off: u16,
        byte: u8,
    ) -> Result<Option<Vec<u8>>, String> {
        let before = uart.out.len();
        uart.write(off, byte)?;
        if uart.out.len() == before {
            return Ok(None);
        }
        self.byte(byte)
    }
    fn byte(&mut self,byte:u8)->Result<Option<Vec<u8>>,String> {
        if byte==b'\n' {let line=std::mem::take(&mut self.line);return Ok(line.starts_with(b"SBX_RPC1 ").then_some(line));}
        if self.line.len()>=MAX_TRANSPORT_FRAME {return Err("management frame too large".into())}
        self.line.push(byte);Ok(None)
    }
    pub fn receive(&mut self,bytes:&[u8])->Result<Vec<Vec<u8>>,String> {
        let mut frames=Vec::new();for byte in bytes {if let Some(frame)=self.byte(*byte)? {frames.push(frame);}}Ok(frames)

    }
}
#[derive(Clone)]
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
    pub error_code: Option<&'static str>,
    pub fatal: bool,
}
#[derive(Clone)]
pub struct Control {
    pub controller: Option<usize>,
    pub latency: u64,
    pub pending: Option<Call>,
    pub log: Vec<Value>,
    pub error: Option<String>,
    names: Vec<String>,
    placement: BTreeMap<String, String>,
    last_relay: u32,
    last_session: u32,
    last_public: u32,
    handles: u32,
    undelivered_reply: bool,
}
impl Control {
    pub fn new(
        config: &Value,
        names: Vec<String>,
        lookahead: u64,
        early: u64,
    ) -> Result<Self, String> {
        let controller = if config.is_null() {
            None
        } else {
            Some(
                names
                    .iter()
                    .position(|n| Some(n.as_str()) == config["machine"].as_str())
                    .ok_or("unknown control machine")?,
            )
        };
        let latency = if controller.is_some() {
            config["latency_ns"].as_u64().ok_or("control latency")?
        } else {
            0
        };
        if controller.is_some()
            && latency
                < lookahead
                    .checked_add(early)
                    .ok_or("control latency overflow")?
        {
            return Err("control latency below causal lookahead".into());
        }
        let placement = if controller.is_some() {
            serde_json::from_value(config["placement"].clone()).map_err(|_| "control placement")?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            controller,
            latency,
            pending: None,
            log: vec![],
            error: None,
            names,
            placement,
            last_relay: 0,
            last_session: 0,
            last_public: 0,
            handles: 0,
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
    fn plan(&mut self, op: &Operation, at: u64) -> Result<(Option<Plan>, Value, u64), String> {
        let mut result = json!({});
        let mut effect = at;
        let action = match op {
            Operation::Stop { target } => Some(Action::Stop {
                machine: self.target(target)?,
            }),
            Operation::Restart { target } => Some(Action::Restart {
                machine: self.target(target)?,
            }),
            Operation::Partition { left, right } => {
                if left.is_empty() || right.is_empty() {
                    return Err("partition sides must be nonempty".into());
                }
                let mut members = BTreeSet::new();
                let mut sides = Vec::new();
                for side in [left, right] {
                    let mut names = Vec::new();
                    for target in side {
                        let name = self.target(target)?;
                        if !members.insert(name.clone()) {
                            return Err(
                                "partition sides must be disjoint with unique machines".into()
                            );
                        }
                        names.push(name);
                    }
                    sides.push(names);
                }
                self.handles = self
                    .handles
                    .checked_add(1)
                    .ok_or("partition handles exhausted")?;
                let handle = format!("p{}", self.handles);
                result = json!({"handle":handle});
                Some(Action::Partition {
                    handle,
                    left: sides.remove(0),
                    right: sides.remove(0),
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
                result = json!({"now_ns":at});
                None
            }
            Operation::Inspect {} => None,
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
        Ok((plan, result, effect))
    }
    pub fn request(
        &mut self,
        machine: usize,
        time: u64,
        line: &[u8],
        max_time: u64,
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
        let session=session.parse::<u32>().map_err(|_|"control session ID")?;
        if session==0 || session<self.last_session {return Err("stale control session ID".into())}
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
        let expected=if session>self.last_session {Some(1)} else {self.last_public.checked_add(1)};
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
        let (operation, plan, result, effect, error) = match request {
            Ok(request) => {
                let operation = request.operation;
                match self.plan(&operation, admitted) {
                    Ok((p, r, e)) => (Some(operation), p, r, e, None),
                    Err(error) => (Some(operation), None, json!({}), admitted, Some(error)),
                }
            }
            Err(error) => (
                None,
                None,
                json!({}),
                admitted,
                Some(format!("invalid request arguments: {error}")),
            ),
        };
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
            error_code: None,
            fatal: false,
        });
        Ok(())
    }
    /// Validate target VM existence at the admission barrier, never at request observation.
    pub fn reject_unavailable(&mut self, exited: &[String]) {
        if let Some(p)=self.pending.as_mut() {
            if !p.admitted_done && p.plan.as_ref().and_then(|plan|plan.action.machine()).is_some_and(|name|exited.iter().any(|n|n==name)) {
                p.plan=None;p.effect=Some(p.admitted);p.error=Some("target VM has exited; restart only starts a stopped process".into());p.error_code=Some("target_unavailable");
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
        let p = self.pending.as_mut().ok_or("no pending control barrier")?;
        if !p.admitted_done {
            p.admitted_done = true;
            if let Some(plan) = p.plan.take() {
                if let Some(error)=faults.admission_error(&plan.action) {
                    p.error=Some(error.into());p.error_code=Some("invalid_state");p.effect=Some(now);
                } else if matches!(&plan.action,Action::Heal{handle} if !faults.active_handles().contains(handle))
                {
                    p.error = Some("heal of inactive partition handle".into());
                    p.effect = Some(now);
                } else {
                    p.fault = Some(faults.append(plan)?);
                }
            }
        }
        if p.effect.is_some_and(|e| e <= now) {
            p.reply = Some(
                p.effect
                    .unwrap()
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
                        Some((crate::faults::Status::Unavailable(at), detail)) => {p.error=Some(detail.to_owned());p.error_code=Some("target_unavailable");Some(at)}
                        _ => None,
                    };
                    if let Some(at) = at {
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
            self.log.push(json!({"relay_id":p.relay,"session_id":p.session,"request":p.request,"status":"cut_off","requested_at_ns":p.requested,"admitted_at_ns":p.admitted,"admitted_done":p.admitted_done,"effect_at_ns":p.effect,"replied_at_ns":p.reply,"cut_off_at_ns":time,"fault_id":p.fault,"plan":plan,"error":p.error,"fatal":p.fatal}));
        }
        for row in self.log.iter_mut().rev().take(1) {
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
    pub fn response(&mut self, time: u64) -> Result<Vec<u8>, String> {
        if self.pending.as_ref().is_none_or(|p| p.reply != Some(time)) {
            return Err("RPC response at wrong virtual time".into());
        }
        let mut p = self.pending.take().unwrap();
        let mut reply = json!({"version":1,"id":p.request["id"],"ok":p.error.is_none(),"result":p.result,"requested_at_ns":p.requested,"admitted_at_ns":p.admitted,"effect_at_ns":p.effect,"replied_at_ns":time});
        if p.error.is_none() && serde_json::to_vec(&reply).map_err(|e|e.to_string())?.len()+1>MAX_FRAME {
            p.error=Some("operation result exceeds response frame bound".into());p.error_code=Some("result_too_large");
        }
        if let Some(error) = p.error {
            reply.as_object_mut().unwrap().remove("result");reply["ok"]=json!(false);
            reply["error"] = json!({"code":if p.fatal {"effect_failed"} else {p.error_code.unwrap_or("invalid_request")},"message":error});
            if p.fatal {self.error = Some(error);}
        }
        if serde_json::to_vec(&reply).map_err(|e|e.to_string())?.len()+1>MAX_FRAME {return Err("control error response exceeds frame bound".into())}
        self.undelivered_reply = true;
        self.log.push(
            json!({"relay_id":p.relay,"session_id":p.session,"request":p.request,"response":reply,"fault_id":p.fault}),
        );
        Ok(format!("SBX_REPLY1 {} {} {}\n", p.relay, p.session, reply).into_bytes())
    }
}
