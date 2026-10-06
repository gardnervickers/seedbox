//! Portable fault state machine. The world advances every runnable machine to
//! each boundary before applying requests. Frames already routed stay queued;
//! partitions drop new sends while any owning handle remains active.
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub struct Faults {
    pub log: Vec<Value>,
    plans: Vec<Value>,
    handles: BTreeMap<String, BTreeSet<(usize, usize)>>,
    names: Vec<String>,
    pub error: Option<String>,
    pub rejected_effects: Vec<Value>,
}
impl Faults {
    pub fn new(plans: Vec<Value>, names: Vec<String>) -> Result<Self, String> {
        if plans.len() > u32::MAX as usize {
            return Err("too many fault requests for effect ID".into());
        }
        let mut previous = 0;
        for (i, p) in plans.iter().enumerate() {
            let at = p["at_ns"].as_u64().ok_or("fault at_ns")?;
            if p["id"].as_u64() != Some((i + 1) as u64)
                || at < previous
                || p["deadline_ns"].as_u64().ok_or("fault deadline")? < at
            {
                return Err("invalid ordered fault identity/time".into());
            }
            previous = at;
            match p["action"].as_str() {
                Some("stop" | "restart") => {
                    if !names
                        .iter()
                        .any(|n| Some(n.as_str()) == p["machine"].as_str())
                    {
                        return Err("unknown fault machine".into());
                    }
                }
                Some("partition") => {
                    if p["handle"].as_str().is_none() {
                        return Err("fault handle".into());
                    }
                    let mut members = BTreeSet::new();
                    for side in ["left", "right"] {
                        let list = p[side].as_array().ok_or("fault partition side")?;
                        if list.is_empty() {
                            return Err("empty fault partition side".into());
                        }
                        for v in list {
                            let n = v.as_str().ok_or("fault member")?;
                            if !names.iter().any(|x| x == n) || !members.insert(n) {
                                return Err("invalid/disjoint fault partition members".into());
                            }
                        }
                    }
                }
                Some("heal") if p["handle"].as_str().is_some() => {}
                _ => return Err("fault action".into()),
            }
        }
        let log = plans.iter().map(|p| json!({"request":p,"id":p["id"],"requested_at_ns":p["at_ns"],"action":p["action"],"machine":p["machine"],"applied_at_ns":null,"status":"unrequested","detail":"scheduled"})).collect();
        Ok(Self {
            log,
            plans,
            handles: BTreeMap::new(),
            names,
            error: None,
            rejected_effects: Vec::new(),
        })
    }
    pub fn append(&mut self, mut plan: Value) -> Result<u32, String> {
        let id = u32::try_from(self.plans.len() + 1).map_err(|_| "fault ID exhausted")?;
        if self
            .plans
            .last()
            .is_some_and(|last| last["at_ns"].as_u64() > plan["at_ns"].as_u64())
        {
            return Err("fault time moved backwards".into());
        }
        plan["id"] = json!(1);
        let mut checked = Self::new(vec![plan], self.names.clone())?;
        let mut plan = checked.plans.pop().unwrap();
        plan["id"] = json!(id);
        let mut row = checked.log.pop().unwrap();
        row["id"] = json!(id);
        row["request"] = plan.clone();
        self.plans.push(plan);
        self.log.push(row);
        Ok(id)
    }
    pub fn next_boundary(&self) -> u64 {
        self.log
            .iter()
            .zip(&self.plans)
            .filter_map(|(r, p)| match r["status"].as_str() {
                Some("unrequested") => p["at_ns"].as_u64(),
                Some("requested") => p["deadline_ns"].as_u64(),
                _ => None,
            })
            .min()
            .unwrap_or(u64::MAX)
    }
    pub fn active_handles(&self) -> Vec<String> {
        self.handles.keys().cloned().collect()
    }
    pub fn blocked(&self, from: usize, to: usize) -> bool {
        self.handles
            .values()
            .any(|pairs| pairs.contains(&(from, to)))
    }
    fn fail(&mut self, i: usize, message: String) {
        self.log[i]["status"] = json!("failed");
        self.log[i]["detail"] = json!(message);
        self.error = Some(message);
    }
    /// Return correlated management commands, after committing the request log.
    pub fn apply_due(&mut self, now: u64) -> Vec<(usize, Vec<u8>)> {
        let mut commands = Vec::new();
        for i in 0..self.plans.len() {
            let p = self.plans[i].clone();
            if self.log[i]["status"] == "requested" && p["deadline_ns"].as_u64().unwrap() <= now {
                self.fail(i, format!("fault {} acknowledgement deadline", i + 1));
            }
            if self.log[i]["status"] != "unrequested" || p["at_ns"].as_u64().unwrap() > now {
                continue;
            }
            self.log[i]["status"] = json!("requested");
            match p["action"].as_str().unwrap() {
                "stop" | "restart" => {
                    let m = self
                        .names
                        .iter()
                        .position(|n| n == p["machine"].as_str().unwrap())
                        .unwrap();
                    let action = if p["action"] == "restart" {
                        "start"
                    } else {
                        "stop"
                    };
                    commands.push((m, format!("{action} {}\n", i + 1).into_bytes()));
                }
                "partition" => {
                    let handle = p["handle"].as_str().unwrap().to_owned();
                    if self.handles.contains_key(&handle) {
                        self.fail(i, "duplicate active partition".into());
                        continue;
                    }
                    let mut pairs = BTreeSet::new();
                    for a in p["left"].as_array().unwrap() {
                        for b in p["right"].as_array().unwrap() {
                            let a = self
                                .names
                                .iter()
                                .position(|n| Some(n.as_str()) == a.as_str())
                                .unwrap();
                            let b = self
                                .names
                                .iter()
                                .position(|n| Some(n.as_str()) == b.as_str())
                                .unwrap();
                            pairs.insert((a, b));
                            pairs.insert((b, a));
                        }
                    }
                    self.handles.insert(handle.clone(), pairs);
                    self.applied(
                        i,
                        now,
                        format!("handle={handle}; in-flight frames retained"),
                    );
                }
                "heal" => {
                    let handle = p["handle"].as_str().unwrap();
                    if self.handles.remove(handle).is_none() {
                        self.fail(i, "heal of inactive partition".into());
                        continue;
                    }
                    self.applied(
                        i,
                        now,
                        format!("handle={handle}; overlapping handles retained"),
                    );
                }
                _ => unreachable!(),
            }
        }
        commands
    }
    fn applied(&mut self, i: usize, time: u64, detail: String) {
        self.log[i]["status"] = json!("applied");
        self.log[i]["applied_at_ns"] = json!(time);
        self.log[i]["detail"] = json!(detail);
    }
    /// The effect port carries the request ID; the VMM supplies machine/time.
    pub fn effect(&mut self, machine: usize, id: u32, time: u64, success: bool) {
        let Some(i) = id
            .checked_sub(1)
            .map(|x| x as usize)
            .filter(|i| *i < self.plans.len())
        else {
            self.rejected_effects.push(json!({"id":id,"machine":self.names[machine],"at_ns":time,"success":success,"status":"rejected","detail":"unknown request ID"}));
            self.error = Some(format!("unexpected fault effect id {id}"));
            return;
        };
        let p = &self.plans[i];
        if self.log[i]["status"] != "requested"
            || p["machine"].as_str() != Some(self.names[machine].as_str())
            || time < p["at_ns"].as_u64().unwrap()
            || time > p["deadline_ns"].as_u64().unwrap()
        {
            self.rejected_effects.push(json!({"id":id,"machine":self.names[machine],"at_ns":time,"success":success,"status":"rejected","detail":"state/machine/time mismatch"}));
            self.fail(
                i,
                format!("stale/duplicate/miscorrelated fault effect {id}"),
            );
            self.log[i]["applied_at_ns"] = Value::Null;
            return;
        }
        if success {
            self.applied(
                i,
                time,
                "guest effect I/O boundary (after reap/exec)".into(),
            );
        } else {
            self.fail(i, format!("guest rejected fault {id}"));
        }
    }
    pub fn complete(&self) -> bool {
        self.error.is_none() && self.log.iter().all(|r| r["status"] == "applied")
    }
}
