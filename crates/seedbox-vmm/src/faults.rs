//! Typed portable fault state. Only outstanding boundaries enter the scheduler;
//! completed history is retained as evidence but never searched to find work.
//! Faults target services. Process faults, and partitions between services that
//! share a machine, are dispatched to that machine's supervisor and complete on
//! its acknowledgement; partitions between machines cut fabric links directly.
use seedbox_control::{Action, Plan};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Scheduled,
    Waiting,
    Applied(u64),
    Failed,
    Unavailable(u64),
}
struct Entry {
    plan: Plan,
    /// Machine whose supervisor applies and acknowledges this fault.
    target: Option<usize>,
    /// Fabric links cut by a partition between machines.
    pairs: BTreeSet<(usize, usize)>,
    status: Status,
    dispatched: bool,
    dispatch_at: Option<u64>,
}
/// An active partition: fabric links, or a cut inside one machine's kernel.
enum Handle {
    Fabric(BTreeSet<(usize, usize)>),
    Local(usize),
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
    stopped: BTreeSet<String>,
    /// Services that exited on their own while their machine kept running.
    exited: BTreeMap<String, u8>,
    pub readiness: Vec<Value>,
    pub exits: Vec<Value>,
    handles: BTreeMap<String, Handle>,
    cuts: BTreeMap<(usize, usize), usize>,
    names: Vec<String>,
    /// Service to machine index.
    placement: BTreeMap<String, usize>,
    changed_network: Vec<usize>,
    pub error: Option<String>,
    pub rejected_effects: Vec<Value>,
}
impl Faults {
    /// Each machine hosts one service of the same name (portable tests).
    #[allow(dead_code)]
    pub fn new(plans: Vec<Value>, names: Vec<String>) -> Result<Self, String> {
        let placement = names.iter().map(|n| (n.clone(), n.clone())).collect();
        Self::with_placement(plans, names, &placement)
    }
    pub fn with_placement(
        plans: Vec<Value>,
        names: Vec<String>,
        placement: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let count = names.len();
        let placement = placement
            .iter()
            .map(|(service, machine)| {
                // Services travel as single tokens in supervisor commands.
                if service.is_empty() || service.bytes().any(|b| b.is_ascii_whitespace() || b == b',') {
                    return Err("invalid placement service name".to_string());
                }
                let m = names.iter().position(|n| n == machine).ok_or("placement names an unknown machine")?;
                Ok((service.clone(), m))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        let mut f = Self {
            log: vec![],
            entries: vec![],
            boundaries: BTreeSet::new(),
            waiting_ready: vec![BTreeSet::new(); count],
            ready: vec![true; count],
            stopped: BTreeSet::new(),
            exited: BTreeMap::new(),
            readiness: vec![],
            exits: vec![],
            handles: BTreeMap::new(),
            cuts: BTreeMap::new(),
            names,
            placement,
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
            Action::Stop { service } | Action::Restart { service } => {
                target = Some(*self.placement.get(service).ok_or("unknown fault service")?);
            }
            Action::Partition {
                handle,
                left,
                right,
            } => {
                if handle.is_empty() || handle.bytes().any(|b| b.is_ascii_whitespace()) || left.is_empty() || right.is_empty() {
                    return Err("empty fault handle/partition side".into());
                }
                let mut members = BTreeSet::new();
                let mut sides = Vec::new();
                for side in [left, right] {
                    let mut indices = Vec::new();
                    for n in side {
                        let m = *self.placement.get(n).ok_or("unknown partition service")?;
                        if !members.insert(n) {
                            return Err("invalid/disjoint fault partition members".into());
                        }
                        indices.push(m);
                    }
                    sides.push(indices);
                }
                let machines: BTreeSet<_> = sides.iter().flatten().copied().collect();
                if machines.len() == 1 {
                    // Co-located services: the shared kernel applies the cut.
                    target = machines.first().copied();
                } else if machines.len() == members.len() {
                    for a in &sides[0] {
                        for b in &sides[1] {
                            pairs.insert((*a, *b));
                            pairs.insert((*b, *a));
                        }
                    }
                } else {
                    return Err("partition mixes co-located and separate services".into());
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
        self.log.push(json!({"request":entry.plan,"id":id,"requested_at_ns":entry.plan.at_ns,"action":entry.plan.action.name(),"service":entry.plan.action.service(),"machine":entry.target.map(|m|&self.names[m]),"applied_at_ns":null,"status":"unrequested","detail":"scheduled"}));
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
    pub fn process_state(&self, service: &str) -> String {
        match self.exited.get(service) {
            Some(code) => format!("Exited({code})"),
            None if self.stopped.contains(service) => "Stopped".into(),
            None => "Running".into(),
        }
    }
    pub fn admission_error(&self, action: &Action) -> Option<&'static str> {
        let service = action.service()?;
        let (stopped, exited) = (self.stopped.contains(service), self.exited.contains_key(service));
        match action {
            Action::Stop{..} if stopped => Some("target process is already stopped"),
            Action::Stop{..} if exited => Some("target process has exited; restart starts it again"),
            Action::Restart{..} if !stopped && !exited => Some("restart requires a stopped or exited target process"),
            _ => None,
        }
    }
    /// A supervisor's `SBX_EXIT1 SERVICE STATUS` notice for a service that
    /// exited on its own while its machine kept running.
    pub fn service_exited(&mut self, machine: usize, time: u64, line: &[u8]) {
        let notice = std::str::from_utf8(line).ok().and_then(|text| text.strip_prefix("SBX_EXIT1 "))
            .and_then(|text| text.split_once(' '))
            .and_then(|(service, code)| Some((service.to_owned(), code.parse::<u8>().ok()?)));
        match notice {
            Some((service, code)) if self.placement.get(&service) == Some(&machine) && !self.stopped.contains(&service) && !self.exited.contains_key(&service) => {
                self.exited.insert(service.clone(), code);
                self.exits.push(json!({"machine":self.names[machine],"service":service,"at_ns":time,"status":code}));
            }
            _ => self.error = Some(format!("invalid service exit notice from {}", self.names[machine])),
        }
    }
    /// Supervisor command for a dispatched fault; services are single tokens.
    fn command(&self, i: usize) -> Vec<u8> {
        let id = self.entries[i].plan.id;
        match &self.entries[i].plan.action {
            Action::Stop { service } => format!("stop {id} {service}\n"),
            Action::Restart { service } => format!("start {id} {service}\n"),
            Action::Partition { handle, left, right } => format!("partition {id} {handle} {} {}\n", left.join(","), right.join(",")),
            Action::Heal { handle } => format!("heal {id} {handle}\n"),
        }
        .into_bytes()
    }
    /// Wait for the target's acknowledgement, dispatching once it is ready.
    fn await_effect(&mut self, i: usize, commands: &mut Vec<(usize, Vec<u8>)>) {
        let id = self.entries[i].plan.id;
        self.boundaries.insert((self.entries[i].plan.deadline_ns, id, Event::Deadline));
        let Some(m) = self.entries[i].target else { return };
        if self.ready[m] {
            self.entries[i].dispatched = true;
            commands.push((m, self.command(i)));
        } else {
            self.waiting_ready[m].insert(id);
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
    /// A target VM that exits before acknowledging leaves its process fault unavailable, not failed.
    pub fn target_exited(&mut self, id: u32, machine: usize, time: u64) {
        let Some(i)=id.checked_sub(1).map(|n|n as usize).filter(|i|*i<self.entries.len()) else {return};
        if self.entries[i].status!=Status::Waiting || self.entries[i].target!=Some(machine) || time<self.entries[i].plan.at_ns || time>self.entries[i].plan.deadline_ns {return}
        self.clear_pending(i);self.entries[i].status=Status::Unavailable(time);
        self.log[i]["status"]=json!("unavailable");self.log[i]["applied_at_ns"]=Value::Null;self.log[i]["detail"]=json!("target VM exited before the process effect");
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
                            self.entries[i].dispatched = true;
                            commands.push((m, self.command(i)));
                        }
                    }
                }
                Event::Apply => {
                    self.entries[i].status = Status::Waiting;
                    self.log[i]["status"] = json!("requested");
                    match self.entries[i].plan.action.clone() {
                        Action::Stop { .. } | Action::Restart { .. } => self.await_effect(i, &mut commands),
                        Action::Partition { handle, .. } => {
                            if self.handles.contains_key(&handle) {
                                self.fail(i, "duplicate active partition".into());
                                continue;
                            }
                            if let Some(m) = self.entries[i].target {
                                self.handles.insert(handle, Handle::Local(m));
                                self.await_effect(i, &mut commands);
                                continue;
                            }
                            let pairs = self.entries[i].pairs.clone();
                            for pair in &pairs {
                                *self.cuts.entry(*pair).or_default() += 1;
                            }
                            self.handles.insert(handle.clone(), Handle::Fabric(pairs));
                            self.applied(
                                i,
                                now,
                                format!("handle={handle}; in-flight frames retained"),
                            );
                            self.changed_network.push(i);
                        }
                        Action::Heal { handle } => {
                            let pairs = match self.handles.remove(&handle) {
                                None => {
                                    self.fail(i, "heal of inactive partition".into());
                                    continue;
                                }
                                Some(Handle::Local(m)) => {
                                    self.entries[i].target = Some(m);
                                    self.log[i]["machine"] = json!(self.names[m]);
                                    self.await_effect(i, &mut commands);
                                    continue;
                                }
                                Some(Handle::Fabric(pairs)) => pairs,
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
            let detail = match &self.entries[i].plan.action {
                Action::Stop { service } => {self.stopped.insert(service.clone()); self.exited.remove(service); "guest effect I/O boundary (after reap)".to_owned()}
                Action::Restart { service } => {self.stopped.remove(service); self.exited.remove(service); "guest effect I/O boundary (after exec)".to_owned()}
                Action::Partition { handle, .. } | Action::Heal { handle } => format!("handle={handle}; guest effect I/O boundary (kernel rules updated)"),
            };
            self.applied(i, time, detail);
        } else {
            self.fail(i, format!("guest rejected fault {id}"));
        }
    }
}
