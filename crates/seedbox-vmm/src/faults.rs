//! Typed portable fault state. Only outstanding boundaries enter the scheduler;
//! completed history is retained as evidence but never searched to find work.
use seedbox_control::{Action, Plan};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Scheduled,
    Waiting,
    Applied(u64),
    Failed,
}
struct Entry {
    plan: Plan,
    target: Option<usize>,
    pairs: BTreeSet<(usize, usize)>,
    status: Status,
    dispatched: bool,
    dispatch_at: Option<u64>,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Apply,
    Dispatch,
    Deadline,
}
pub struct Faults {
    pub log: Vec<Value>,
    entries: Vec<Entry>,
    boundaries: BTreeSet<(u64, u32, Event)>,
    waiting_ready: Vec<BTreeSet<u32>>,
    ready: Vec<bool>,
    stopped: Vec<bool>,
    pub readiness: Vec<Value>,
    handles: BTreeMap<String, BTreeSet<(usize, usize)>>,
    cuts: BTreeMap<(usize, usize), usize>,
    names: Vec<String>,
    changed_network: Vec<usize>,
    pub error: Option<String>,
    pub rejected_effects: Vec<Value>,
}
impl Faults {
    pub fn new(plans: Vec<Value>, names: Vec<String>) -> Result<Self, String> {
        let count = names.len();
        let mut f = Self {
            log: vec![],
            entries: vec![],
            boundaries: BTreeSet::new(),
            waiting_ready: vec![BTreeSet::new(); count],
            ready: vec![true; count],
            stopped: vec![false; count],
            readiness: vec![],
            handles: BTreeMap::new(),
            cuts: BTreeMap::new(),
            names,
            changed_network: vec![],
            error: None,
            rejected_effects: vec![],
        };
        for (i, value) in plans.into_iter().enumerate() {
            let plan: Plan =
                serde_json::from_value(value).map_err(|e| format!("invalid fault plan: {e}"))?;
            if plan.id as usize != i + 1 {
                return Err("invalid ordered fault identity/time".into());
            }
            f.append(plan)?;
        }
        Ok(f)
    }
    pub fn require_readiness(&mut self) {
        self.ready.fill(false);
    }
    fn compile(&self, plan: Plan) -> Result<Entry, String> {
        if plan.deadline_ns < plan.at_ns {
            return Err("fault deadline precedes request".into());
        }
        let mut target = None;
        let mut pairs = BTreeSet::new();
        match &plan.action {
            Action::Stop { machine } | Action::Restart { machine } => {
                target = Some(
                    self.names
                        .iter()
                        .position(|n| n == machine)
                        .ok_or("unknown fault machine")?,
                );
            }
            Action::Partition {
                handle,
                left,
                right,
            } => {
                if handle.is_empty() || left.is_empty() || right.is_empty() {
                    return Err("empty fault handle/partition side".into());
                }
                let mut members = BTreeSet::new();
                let mut sides = Vec::new();
                for side in [left, right] {
                    let mut indices = Vec::new();
                    for n in side {
                        let m = self
                            .names
                            .iter()
                            .position(|x| x == n)
                            .ok_or("unknown partition machine")?;
                        if !members.insert(m) {
                            return Err("invalid/disjoint fault partition members".into());
                        }
                        indices.push(m);
                    }
                    sides.push(indices);
                }
                for a in &sides[0] {
                    for b in &sides[1] {
                        pairs.insert((*a, *b));
                        pairs.insert((*b, *a));
                    }
                }
            }
            Action::Heal { handle } => {
                if handle.is_empty() {
                    return Err("empty fault handle".into());
                }
            }
        }
        Ok(Entry {
            plan,
            target,
            pairs,
            status: Status::Scheduled,
            dispatched: false,
            dispatch_at: None,
        })
    }
    pub fn append(&mut self, mut plan: Plan) -> Result<u32, String> {
        let id = u32::try_from(self.entries.len() + 1).map_err(|_| "fault ID exhausted")?;
        if self
            .entries
            .last()
            .is_some_and(|e| e.plan.at_ns > plan.at_ns)
        {
            return Err("fault time moved backwards".into());
        }
        plan.id = id;
        let entry = self.compile(plan)?;
        self.log.push(json!({"request":entry.plan,"id":id,"requested_at_ns":entry.plan.at_ns,"action":entry.plan.action.name(),"machine":entry.plan.action.machine(),"applied_at_ns":null,"status":"unrequested","detail":"scheduled"}));
        self.boundaries.insert((entry.plan.at_ns, id, Event::Apply));
        self.entries.push(entry);
        Ok(id)
    }
    pub fn next_boundary(&self) -> u64 {
        self.boundaries.first().map(|e| e.0).unwrap_or(u64::MAX)
    }
    pub fn pending_boundaries(&self) -> usize {
        self.boundaries.len()
    }
    pub fn plan(&self, id: u32) -> Option<&Plan> {
        id.checked_sub(1)
            .and_then(|n| self.entries.get(n as usize))
            .map(|e| &e.plan)
    }
    pub fn outcome(&self, id: u32) -> Option<(Status, &str)> {
        id.checked_sub(1)
            .and_then(|n| self.entries.get(n as usize))
            .map(|e| {
                (
                    e.status,
                    self.log[id as usize - 1]["detail"]
                        .as_str()
                        .unwrap_or("fault failed"),
                )
            })
    }
    pub fn active_handles(&self) -> Vec<String> {
        self.handles.keys().cloned().collect()
    }
    pub fn process_state(&self, machine: usize) -> &'static str {
        if self.stopped[machine] {"Stopped"} else {"Running"}
    }
    pub fn admission_error(&self, action: &Action) -> Option<&'static str> {
        let machine = action.machine()?;
        let index = self.names.iter().position(|name|name==machine)?;
        match action {
            Action::Stop{..} if self.stopped[index] => Some("target process is already stopped"),
            Action::Restart{..} if !self.stopped[index] => Some("restart requires a stopped target process"),
            _ => None,
        }
    }
    pub fn blocked(&self, from: usize, to: usize) -> bool {
        self.cuts.contains_key(&(from, to))
    }
    pub fn take_changed_network(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.changed_network)
    }
    fn clear_pending(&mut self, i: usize) {
        let e = &self.entries[i];
        let id = e.plan.id;
        self.boundaries.remove(&(e.plan.at_ns, id, Event::Apply));
        self.boundaries
            .remove(&(e.plan.deadline_ns, id, Event::Deadline));
        if let Some(t) = e.dispatch_at {
            self.boundaries.remove(&(t, id, Event::Dispatch));
        }
        if let Some(m) = e.target {
            self.waiting_ready[m].remove(&id);
        }
    }
    fn fail(&mut self, i: usize, message: String) {
        self.clear_pending(i);
        self.entries[i].status = Status::Failed;
        self.log[i]["status"] = json!("failed");
        self.log[i]["applied_at_ns"] = Value::Null;
        self.log[i]["detail"] = json!(message);
        self.error = Some(message);
    }
    fn applied(&mut self, i: usize, time: u64, detail: String) {
        self.clear_pending(i);
        self.entries[i].status = Status::Applied(time);
        self.log[i]["status"] = json!("applied");
        self.log[i]["applied_at_ns"] = json!(time);
        self.log[i]["detail"] = json!(detail);
    }
    /// Readiness is the reserved success id0; process commands wait for it.
    pub fn management_ready(&mut self, m: usize, time: u64, latency: u64) -> Result<(), String> {
        if m >= self.names.len() || self.ready[m] {
            return Err("duplicate/unknown management readiness".into());
        }
        self.ready[m] = true;
        self.readiness
            .push(json!({"machine":self.names[m],"at_ns":time}));
        for id in std::mem::take(&mut self.waiting_ready[m]) {
            let dispatch = time
                .checked_add(latency)
                .ok_or("management dispatch overflow")?;
            let e = &self.entries[id as usize - 1];
            if dispatch < e.plan.deadline_ns {
                self.boundaries.insert((dispatch, id, Event::Dispatch));
                self.entries[id as usize - 1].dispatch_at = Some(dispatch);
            }
        }
        Ok(())
    }
    pub fn apply_due(&mut self, now: u64) -> Vec<(usize, Vec<u8>)> {
        let mut commands = Vec::new();
        while self.boundaries.first().is_some_and(|b| b.0 <= now) {
            let Some((_, id, event)) = self.boundaries.pop_first() else {
                break;
            };
            let i = id as usize - 1;
            match event {
                Event::Deadline => {
                    if self.entries[i].status == Status::Waiting {
                        self.fail(i, format!("fault {id} acknowledgement deadline"));
                    }
                }
                Event::Dispatch => {
                    if self.entries[i].status == Status::Waiting {
                        if let Some(m) = self.entries[i].target {
                            let action =
                                if matches!(self.entries[i].plan.action, Action::Restart { .. }) {
                                    "start"
                                } else {
                                    "stop"
                                };
                            self.entries[i].dispatched = true;
                            commands.push((m, format!("{action} {id}\n").into_bytes()));
                        }
                    }
                }
                Event::Apply => {
                    self.entries[i].status = Status::Waiting;
                    self.log[i]["status"] = json!("requested");
                    match self.entries[i].plan.action.clone() {
                        Action::Stop { .. } | Action::Restart { .. } => {
                            self.boundaries.insert((
                                self.entries[i].plan.deadline_ns,
                                id,
                                Event::Deadline,
                            ));
                            if let Some(m) = self.entries[i].target {
                                if self.ready[m] {
                                    let action = if matches!(
                                        self.entries[i].plan.action,
                                        Action::Restart { .. }
                                    ) {
                                        "start"
                                    } else {
                                        "stop"
                                    };
                                    self.entries[i].dispatched = true;
                                    commands.push((m, format!("{action} {id}\n").into_bytes()));
                                } else {
                                    self.waiting_ready[m].insert(id);
                                }
                            }
                        }
                        Action::Partition { handle, .. } => {
                            if self.handles.contains_key(&handle) {
                                self.fail(i, "duplicate active partition".into());
                                continue;
                            }
                            let pairs = self.entries[i].pairs.clone();
                            for pair in &pairs {
                                *self.cuts.entry(*pair).or_default() += 1;
                            }
                            self.handles.insert(handle.clone(), pairs);
                            self.applied(
                                i,
                                now,
                                format!("handle={handle}; in-flight frames retained"),
                            );
                            self.changed_network.push(i);
                        }
                        Action::Heal { handle } => {
                            let Some(pairs) = self.handles.remove(&handle) else {
                                self.fail(i, "heal of inactive partition".into());
                                continue;
                            };
                            for pair in pairs {
                                if let Some(n) = self.cuts.get_mut(&pair) {
                                    *n -= 1;
                                    if *n == 0 {
                                        self.cuts.remove(&pair);
                                    }
                                }
                            }
                            self.applied(
                                i,
                                now,
                                format!("handle={handle}; overlapping handles retained"),
                            );
                            self.changed_network.push(i);
                        }
                    }
                }
            }
        }
        commands
    }
    pub fn effect(&mut self, machine: usize, id: u32, time: u64, success: bool) {
        let Some(i) = id
            .checked_sub(1)
            .map(|n| n as usize)
            .filter(|i| *i < self.entries.len())
        else {
            self.rejected_effects.push(json!({"id":id,"machine":self.names.get(machine),"at_ns":time,"success":success,"status":"rejected","detail":"unknown request ID"}));
            self.error = Some(format!("unexpected fault effect id {id}"));
            return;
        };
        let e = &self.entries[i];
        if e.status != Status::Waiting
            || !e.dispatched
            || e.target != Some(machine)
            || time < e.plan.at_ns
            || time > e.plan.deadline_ns
        {
            self.rejected_effects.push(json!({"id":id,"machine":self.names.get(machine),"at_ns":time,"success":success,"status":"rejected","detail":"state/machine/time mismatch"}));
            self.fail(
                i,
                format!("stale/duplicate/miscorrelated fault effect {id}"),
            );
            return;
        }
        if success {
            self.stopped[machine]=matches!(self.entries[i].plan.action,Action::Stop{..});
            self.applied(
                i,
                time,
                "guest effect I/O boundary (after reap/exec)".into(),
            );
        } else {
            self.fail(i, format!("guest rejected fault {id}"));
        }
    }
}
