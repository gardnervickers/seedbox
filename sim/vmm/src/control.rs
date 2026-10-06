//! Language-neutral runtime control. Fixed latency is semantic, never selected
//! from host performance or another machine's observed progress.
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_FRAME: usize = 16384;
pub const MAX_TRANSPORT_FRAME: usize = MAX_FRAME + 64;
/// Frame only actual UART transmissions, excluding divisor-register writes.
#[derive(Default)]
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
}
#[derive(Clone)]
pub struct Call {
    pub relay: u32,
    pub request: Value,
    pub requested: u64,
    pub admitted: u64,
    pub admitted_done: bool,
    pub effect: Option<u64>,
    pub reply: Option<u64>,
    pub fault: Option<u32>,
    pub plan: Option<Value>,
    pub result: Value,
    pub error: Option<String>,
    pub fatal: bool,
}
pub struct Control {
    pub controller: Option<usize>,
    pub latency: u64,
    pub pending: Option<Call>,
    pub log: Vec<Value>,
    pub error: Option<String>,
    names: Vec<String>,
    placement: BTreeMap<String, String>,
    last_relay: u32,
    last_public: u32,
    handles: u32,
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
            last_public: 0,
            handles: 0,
        })
    }
    fn target(&self, value: &Value) -> Result<String, String> {
        let map = value.as_object().ok_or("target must be object")?;
        if map.len() != 1 {
            return Err("target needs exactly one service or machine".into());
        }
        let name = if let Some(service) = map.get("service") {
            self.placement
                .get(service.as_str().ok_or("service must be string")?)
                .cloned()
                .ok_or("unknown service")?
        } else {
            map.get("machine")
                .and_then(Value::as_str)
                .ok_or("unknown target kind")?
                .to_owned()
        };
        if !self.names.contains(&name) {
            return Err("unknown machine".into());
        }
        Ok(name)
    }
    fn plan(
        &mut self,
        r: &Value,
        at: u64,
        duration: u64,
    ) -> Result<(Option<Value>, Value, u64), String> {
        let op = r["op"].as_str().ok_or("op must be string")?;
        let fields: &[&str] = match op {
            "stop" | "restart" => &["target"],
            "partition" => &["left", "right"],
            "heal" => &["handle"],
            "sleep" => &["duration_ns"],
            "now" | "inspect" => &[],
            _ => return Err("unknown operation".into()),
        };
        let map = r.as_object().ok_or("request must be object")?;
        if map.len() != 3 + fields.len()
            || map.keys().any(|k| {
                !["version", "id", "op"].contains(&k.as_str()) && !fields.contains(&k.as_str())
            })
        {
            return Err("unexpected or missing request field".into());
        }
        let mut result = json!({});
        let mut effect = at;
        let mut p = match op {
            "stop" | "restart" => {
                let machine = self.target(&r["target"])?;
                if Some(machine.as_str()) == self.controller.map(|i| self.names[i].as_str()) {
                    return Err("cannot stop/restart calling controller".into());
                }
                json!({"action":op,"machine":machine})
            }
            "partition" => {
                let mut members = BTreeSet::new();
                let mut sides = Vec::new();
                for key in ["left", "right"] {
                    let side = r[key].as_array().ok_or("partition side must be array")?;
                    if side.is_empty() {
                        return Err("partition sides must be nonempty".into());
                    }
                    let mut names = Vec::new();
                    for target in side {
                        let name = self.target(target)?;
                        if Some(name.as_str()) == self.controller.map(|i| self.names[i].as_str()) {
                            return Err("controller is outside partition failure model".into());
                        }
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
                json!({"action":op,"handle":handle,"left":sides[0],"right":sides[1]})
            }
            "heal" => {
                json!({"action":op,"handle":r["handle"].as_str().filter(|s| !s.is_empty()).ok_or("handle must be nonempty string")?})
            }
            "sleep" => {
                effect = at
                    .checked_add(
                        r["duration_ns"]
                            .as_u64()
                            .ok_or("duration_ns must be unsigned integer")?,
                    )
                    .ok_or("sleep overflow")?;
                Value::Null
            }
            "now" => {
                result = json!({"now_ns":at});
                Value::Null
            }
            "inspect" => Value::Null,
            _ => unreachable!(),
        };
        if effect
            .checked_add(self.latency)
            .filter(|t| *t < duration)
            .is_none()
        {
            return Err("reply would exceed simulation limit".into());
        }
        if !p.is_null() {
            p["at_ns"] = json!(at);
            p["deadline_ns"] = json!(at
                .saturating_add(5_000_000_000)
                .min(duration.saturating_sub(self.latency + 1)));
        }
        Ok(((!p.is_null()).then_some(p), result, effect))
    }
    pub fn request(
        &mut self,
        machine: usize,
        time: u64,
        line: &[u8],
        duration: u64,
        max_time: u64,
    ) -> Result<(), String> {
        if self.controller != Some(machine) {
            return Err("control request from unauthorized machine".into());
        }
        if self.pending.is_some() {
            return Err("control request while another is outstanding".into());
        }
        if line.len() > MAX_TRANSPORT_FRAME {
            return Err("control frame too large".into());
        }
        let text = std::str::from_utf8(line).map_err(|_| "control frame UTF-8")?;
        let tail = text.strip_prefix("SBX_RPC1 ").ok_or("control prefix")?;
        let (relay, payload) = tail.split_once(' ').ok_or("control frame envelope")?;
        if payload.len() + 1 > MAX_FRAME {
            return Err("public control JSON frame too large".into());
        }
        let relay = relay.parse::<u32>().map_err(|_| "control relay ID")?;
        if relay == 0 || relay != self.last_relay.checked_add(1).ok_or("relay ID exhausted")? {
            return Err("stale/nonsequential control relay ID".into());
        }
        let r: Value = serde_json::from_str(payload).map_err(|_| "malformed control JSON")?;
        if r["version"] != 1
            || r["id"]
                .as_u64()
                .filter(|id| *id > 0 && *id <= u32::MAX as u64)
                .is_none()
        {
            return Err("control version/id".into());
        }
        let public = r["id"].as_u64().unwrap() as u32;
        if Some(public) != self.last_public.checked_add(1) {
            return Err("stale/nonsequential public control ID".into());
        }
        self.last_public = public;
        let admitted = time
            .checked_add(self.latency)
            .filter(|t| *t < duration)
            .ok_or("control admission exceeds simulation limit")?;
        if max_time > admitted {
            return Err("control admission is in another machine's past".into());
        }
        self.last_relay = relay;
        let (plan, result, effect, error) = match self.plan(&r, admitted, duration) {
            Ok((p, r, e)) => (p, r, e, None),
            Err(e) => (None, json!({}), admitted, Some(e)),
        };
        self.pending = Some(Call {
            relay,
            request: r,
            requested: time,
            admitted,
            admitted_done: false,
            effect: plan.is_none().then_some(effect),
            reply: None,
            fault: None,
            plan,
            result,
            error,
            fatal: false,
        });
        Ok(())
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
        duration: u64,
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
                if plan["action"] == "heal"
                    && !faults
                        .active_handles()
                        .iter()
                        .any(|h| Some(h.as_str()) == plan["handle"].as_str())
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
                    .filter(|r| *r < duration)
                    .ok_or("control reply exceeds limit")?,
            );
        }
        Ok(None)
    }
    pub fn observe_faults(
        &mut self,
        faults: &crate::faults::Faults,
        time: u64,
        duration: u64,
    ) -> Result<(), String> {
        if let Some(p) = self.pending.as_mut() {
            if p.effect.is_none() {
                if let Some(id) = p.fault {
                    let row = &faults.log[id as usize - 1];
                    let at = if let Some(at) = row["applied_at_ns"].as_u64() {
                        Some(at)
                    } else if row["status"] == "failed" {
                        p.error = Some(row["detail"].as_str().unwrap_or("fault failed").to_owned());
                        p.fatal = true;
                        Some(time)
                    } else {
                        None
                    };
                    if let Some(at) = at {
                        p.effect = Some(at);
                        p.reply = Some(
                            at.checked_add(self.latency)
                                .filter(|r| *r < duration)
                                .ok_or("control reply exceeds simulation limit")?,
                        );
                    }
                }
            }
        }
        Ok(())
    }
    pub fn has_undelivered_reply(&self) -> bool {
        self.log
            .iter()
            .any(|r| r["response"].is_object() && r["delivered_at_ns"].is_null())
    }
    pub fn complete(&self) -> bool {
        self.pending.is_none()
            && self.error.is_none()
            && self
                .log
                .iter()
                .all(|r| r["delivered_at_ns"].as_u64() == r["response"]["replied_at_ns"].as_u64())
    }
    pub fn response(&mut self, time: u64) -> Result<Vec<u8>, String> {
        if self.pending.as_ref().is_none_or(|p| p.reply != Some(time)) {
            return Err("RPC response at wrong virtual time".into());
        }
        let mut p = self.pending.take().unwrap();
        if serde_json::to_vec(&p.result)
            .map_err(|e| e.to_string())?
            .len()
            + 512
            > MAX_FRAME
        {
            p.error = Some("operation result exceeds response frame bound".into());
            p.result = json!({});
        }
        let mut reply = json!({"version":1,"id":p.request["id"],"ok":p.error.is_none(),"result":p.result,"requested_at_ns":p.requested,"admitted_at_ns":p.admitted,"effect_at_ns":p.effect,"replied_at_ns":time});
        if let Some(error) = p.error {
            reply.as_object_mut().unwrap().remove("result");
            reply["error"] = json!({"code":if p.fatal {"effect_failed"} else {"invalid_request"},"message":error});
            if p.fatal {
                self.error = Some(error);
            }
        }
        self.log
            .push(json!({"relay_id":p.relay,"request":p.request,"response":reply}));
        Ok(format!("SBX_REPLY1 {} {}\n", p.relay, reply).into_bytes())
    }
}
