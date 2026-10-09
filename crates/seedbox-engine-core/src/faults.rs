//! Typed portable fault state. Only outstanding boundaries enter the scheduler;
//! completed history is retained as evidence but never searched to find work.
use crate::links::{Links, Policy};
use seedbox_control::{Action, Plan};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Scheduled,
    Waiting,
    Applied(u64),
    Failed,
    Unavailable(u64),
}
#[derive(Clone)]
struct Entry {
    plan: Plan,
    target: Option<usize>,
    pairs: BTreeSet<(usize, usize)>,
    status: Status,
    dispatched: bool,
    dispatch_at: Option<u64>,
}
/// Whole-machine lifecycle. A paused machine keeps its VM and executes
/// nothing; a down machine has no VM until it reboots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    Up,
    Paused,
    Down,
}
/// A lifecycle transition applied at a fault boundary, for the world to carry
/// out on the machine before anything else happens at that time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Pause,
    Resume,
    Crash,
    Reboot,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Apply,
    Dispatch,
    Deadline,
}
#[derive(Clone)]
pub struct Faults {
    pub log: Vec<Value>,
    pub links: Links,
    pub connections: crate::connections::Connections,
    entries: Vec<Entry>,
    boundaries: BTreeSet<(u64, u32, Event)>,
    waiting_ready: Vec<BTreeSet<u32>>,
    ready: Vec<bool>,
    stopped: Vec<bool>,
    life: Vec<Life>,
    /// Outstanding lifecycle requests per machine, by time: nothing may be
    /// delivered early to a machine across one of these.
    lifecycle_at: Vec<BTreeSet<(u64, u32)>>,
    lifecycle: Vec<(usize, Lifecycle)>,
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
        let links = Links::new(
            0,
            &names,
            Policy {
                latency_ns: 1,
                jitter_ns: 0,
                loss_ppm: 0,
                bandwidth_bps: None,
                queue_bound_ns: None,
            },
            BTreeMap::new(),
        )?;
        Self::with_links(plans, names, links)
    }
    pub fn with_links(plans: Vec<Value>, names: Vec<String>, links: Links) -> Result<Self, String> {
        let count = names.len();
        let mut f = Self {
            log: vec![],
            links,
            connections: crate::connections::Connections::default(),
            entries: vec![],
            boundaries: BTreeSet::new(),
            waiting_ready: vec![BTreeSet::new(); count],
            ready: vec![true; count],
            stopped: vec![false; count],
            life: vec![Life::Up; count],
            lifecycle_at: vec![BTreeSet::new(); count],
            lifecycle: vec![],
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
    /// A standby machine starts down and reports readiness when it boots.
    pub fn start_down(&mut self, m: usize) {
        self.life[m] = Life::Down;
        self.ready[m] = false;
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
            Action::Stop { machine }
            | Action::Restart { machine }
            | Action::Pause { machine }
            | Action::Resume { machine }
            | Action::Crash { machine }
            | Action::Reboot { machine } => {
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
            } => {
                Policy {
                    latency_ns: *latency_ns,
                    jitter_ns: *jitter_ns,
                    loss_ppm: *loss_ppm,
                    bandwidth_bps: *bandwidth_bps,
                    queue_bound_ns: *queue_bound_ns,
                }
                .validate(self.links.global.latency_ns)?;
                network_faults.validate(*latency_ns, *jitter_ns)?;
                if handle.is_empty() || from.is_empty() || to.is_empty() {
                    return Err("empty fault handle/shape side".into());
                }
                let mut members = BTreeSet::new();
                let mut sides = Vec::new();
                for side in [from, to] {
                    let mut indices = Vec::new();
                    for name in side {
                        let m = self
                            .names
                            .iter()
                            .position(|n| n == name)
                            .ok_or("unknown shape machine")?;
                        if !members.insert(m) {
                            return Err("shape sides must be disjoint with unique machines".into());
                        }
                        indices.push(m);
                    }
                    sides.push(indices);
                }
                for &a in &sides[0] {
                    for &b in &sides[1] {
                        pairs.insert((a, b));
                    }
                }
            }
            Action::DropConnection { handle, connection } => {
                if handle.is_empty()
                    || connection.incarnation == 0
                    || connection.src_port == 0
                    || connection.dst_port == 0
                    || connection.from == connection.to
                    || !self.names.contains(&connection.from)
                    || !self.names.contains(&connection.to)
                {
                    return Err("invalid connection selector".into());
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
        if let (Some(m), Some(_)) = (entry.target, lifecycle(&entry.plan.action)) {
            self.lifecycle_at[m].insert((entry.plan.at_ns, id));
        }
        self.entries.push(entry);
        Ok(id)
    }
    pub fn next_boundary(&self) -> u64 {
        self.boundaries.first().map(|e| e.0).unwrap_or(u64::MAX)
    }
    /// The earliest outstanding lifecycle request for machine `m`.
    pub fn next_lifecycle(&self, m: usize) -> u64 {
        self.lifecycle_at[m]
            .first()
            .map(|e| e.0)
            .unwrap_or(u64::MAX)
    }
    #[allow(dead_code)] // The world reads Machine::paused/down; tests read this.
    pub fn life(&self, m: usize) -> Life {
        self.life[m]
    }
    /// Lifecycle transitions applied since the last call, in order.
    pub fn take_lifecycle(&mut self) -> Vec<(usize, Lifecycle)> {
        std::mem::take(&mut self.lifecycle)
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
        let mut handles: Vec<_> = self
            .handles
            .keys()
            .cloned()
            .chain(self.links.layers.values().map(|l| l.handle.clone()))
            .collect();
        handles.extend(self.connections.selections.keys().cloned());
        handles.sort();
        handles
    }
    pub fn process_state(&self, machine: usize) -> &'static str {
        match self.life[machine] {
            Life::Paused => "Paused",
            Life::Down => "Down",
            Life::Up if self.stopped[machine] => "Stopped",
            Life::Up => "Running",
        }
    }
    pub fn admission_error(&self, action: &Action) -> Option<&'static str> {
        if let Action::DropConnection { connection, .. } = action {
            if self.connections.selections.len() == crate::connections::CAPACITY {
                return Some("connection selector capacity exhausted");
            }
            return (!self.connections.available(connection, &self.names))
                .then_some("connection incarnation unavailable");
        }
        let machine = action.machine()?;
        let index = self.names.iter().position(|name| name == machine)?;
        match action {
            // Down targets are admitted so process faults resolve as unavailable.
            Action::Stop { .. } | Action::Restart { .. } if self.life[index] == Life::Down => None,
            // A paused guest cannot acknowledge; its deadline would fail the run.
            Action::Stop { .. } | Action::Restart { .. } if self.life[index] == Life::Paused => {
                Some("target machine is paused")
            }
            Action::Stop { .. } if self.stopped[index] => Some("target process is already stopped"),
            Action::Restart { .. } if !self.stopped[index] => {
                Some("restart requires a stopped target process")
            }
            Action::Pause { .. } if self.life[index] != Life::Up => {
                Some("target machine is not running")
            }
            Action::Resume { .. } if self.life[index] != Life::Paused => {
                Some("target machine is not paused")
            }
            Action::Crash { .. } if self.life[index] == Life::Down => {
                Some("target machine is already down")
            }
            Action::Reboot { .. } if self.life[index] != Life::Down => {
                Some("target machine is not down")
            }
            _ => None,
        }
    }
    pub fn network_witness(&self, h: &mut Sha256) {
        self.links.witness(h);
        self.connections.witness(h);
        h.update((self.handles.len() as u64).to_le_bytes());
        for (handle, pairs) in &self.handles {
            crate::links::string_hash(h, handle);
            h.update((pairs.len() as u64).to_le_bytes());
            for &pair in pairs {
                crate::links::pair_hash(h, pair);
            }
        }
    }
    pub fn connections(
        &self,
        from: &str,
        to: &str,
        after: u64,
    ) -> Vec<seedbox_control::Connection> {
        self.connections.list(from, to, after, &self.names)
    }
    pub fn route(
        &mut self,
        src: usize,
        dst: usize,
        send: u64,
        frame: &[u8],
        legacy_cut: bool,
    ) -> crate::links::Outcome {
        let c = self.connections.observe(src, dst, frame);
        let cut = legacy_cut || self.blocked(src, dst);
        // Only selector rows grow selection evidence; legacy logs stay identical.
        let drop = self.connections.drops(c, cut, |s| {
            self.log[s.fault_id as usize - 1]["selection_counts"] =
                json!({"matched":s.matched,"dropped":s.dropped});
        });
        if drop {
            self.links.selected_drop(src, dst);
            return crate::links::Outcome::Lost;
        }
        let outcome = self.links.decide(src, dst, send, frame.len(), cut);
        self.record_link_counts(src, dst);
        outcome
    }
    pub fn deliveries(
        &mut self,
        src: usize,
        dst: usize,
        at: u64,
        frame: &[u8],
    ) -> Vec<(u64, Vec<u8>)> {
        let deliveries = self.links.deliveries((src, dst), at, frame);
        if deliveries[0].1 != frame && deliveries[0].1.get(47) == Some(&0x14) {
            self.connections.observe(src, dst, &deliveries[0].1);
        }
        self.record_link_counts(src, dst);
        deliveries
    }
    fn record_link_counts(&mut self, src: usize, dst: usize) {
        if let Some((&id, layer)) = self
            .links
            .layers
            .iter()
            .rev()
            .find(|(_, l)| l.pairs.contains(&(src, dst)))
        {
            if layer.policy.bandwidth_bps.is_some() {
                self.log[id as usize - 1]["queue_counts"] = json!(layer.queue_counts);
            }
            if !layer.network_faults.is_off() {
                self.log[id as usize - 1]["network_fault_counts"] = json!(layer.network_counts);
            }
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
            self.lifecycle_at[m].remove(&(e.plan.at_ns, id));
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
        let Some(i) = id
            .checked_sub(1)
            .map(|n| n as usize)
            .filter(|i| *i < self.entries.len())
        else {
            return;
        };
        if self.entries[i].status != Status::Waiting
            || self.entries[i].target != Some(machine)
            || time < self.entries[i].plan.at_ns
            || time > self.entries[i].plan.deadline_ns
        {
            return;
        }
        self.clear_pending(i);
        self.entries[i].status = Status::Unavailable(time);
        self.log[i]["status"] = json!("unavailable");
        self.log[i]["applied_at_ns"] = Value::Null;
        self.log[i]["detail"] = json!("target VM exited before the process effect");
    }
    fn unavailable(&mut self, i: usize, time: u64, detail: &str) {
        self.clear_pending(i);
        self.entries[i].status = Status::Unavailable(time);
        self.log[i]["status"] = json!("unavailable");
        self.log[i]["applied_at_ns"] = Value::Null;
        self.log[i]["detail"] = json!(detail);
    }
    /// The machine's incarnation is gone. Its process faults can no longer
    /// take effect, and the next incarnation reports readiness afresh, so
    /// nothing the old one was sent or acknowledged carries over.
    fn machine_down(&mut self, m: usize, time: u64) {
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            if e.target == Some(m)
                && e.status == Status::Waiting
                && lifecycle(&e.plan.action).is_none()
            {
                self.unavailable(i, time, "target machine crashed before the process effect");
            }
        }
        self.ready[m] = false;
        self.stopped[m] = false;
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
                        Action::Stop { .. } | Action::Restart { .. }
                            if self.entries[i]
                                .target
                                .is_some_and(|m| self.life[m] == Life::Down) =>
                        {
                            self.unavailable(i, now, "target machine is down");
                        }
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
                            if self.active_handles().contains(&handle) {
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
                        Action::Shape {
                            handle,
                            latency_ns,
                            jitter_ns,
                            loss_ppm,
                            bandwidth_bps,
                            queue_bound_ns,
                            network_faults,
                            ..
                        } => {
                            if self.active_handles().contains(&handle) {
                                self.fail(i, "duplicate active handle".into());
                                continue;
                            }
                            if let Err(error) = self.links.shape_with_faults(
                                id,
                                handle.clone(),
                                self.entries[i].pairs.clone(),
                                Policy {
                                    latency_ns,
                                    jitter_ns,
                                    loss_ppm,
                                    bandwidth_bps,
                                    queue_bound_ns,
                                },
                                network_faults,
                            ) {
                                self.fail(i, error);
                                continue;
                            }
                            if bandwidth_bps.is_some() {
                                self.log[i]["queue_counts"] =
                                    json!(crate::links::QueueCounts::default());
                            }
                            if !network_faults.is_off() {
                                self.log[i]["network_fault_counts"] =
                                    json!(crate::network_faults::Counts::default());
                            }
                            self.applied(
                                i,
                                now,
                                format!("handle={handle}; in-flight frames retained"),
                            );
                            self.changed_network.push(i);
                        }
                        Action::DropConnection { handle, connection } => {
                            if self.active_handles().contains(&handle) {
                                self.fail(i, "duplicate active handle".into());
                                continue;
                            }
                            if let Err(error) =
                                self.connections
                                    .select(id, handle.clone(), connection, &self.names)
                            {
                                self.fail(i, error);
                                continue;
                            }
                            self.log[i]["selection_counts"] = json!({"matched":0,"dropped":0});
                            self.applied(
                                i,
                                now,
                                format!("handle={handle}; armed; in-flight frames retained"),
                            );
                        }
                        Action::Heal { handle } => {
                            if let Some(selection) = self.connections.selections.remove(&handle) {
                                self.log[i]["selection_counts"] = json!({"matched":selection.matched,"dropped":selection.dropped});
                                self.applied(
                                    i,
                                    now,
                                    format!(
                                        "handle={handle}; selected packets dropped={}",
                                        selection.dropped
                                    ),
                                );
                                continue;
                            }
                            let queue_counts = self
                                .links
                                .layers
                                .values()
                                .find(|l| l.handle == handle && l.policy.bandwidth_bps.is_some())
                                .map(|l| l.queue_counts);
                            let network_counts = self
                                .links
                                .layers
                                .values()
                                .find(|l| l.handle == handle && !l.network_faults.is_off())
                                .map(|l| l.network_counts);
                            if self.links.heal(&handle) {
                                if let Some(counts) = queue_counts {
                                    self.log[i]["queue_counts"] = json!(counts);
                                }
                                if let Some(counts) = network_counts {
                                    self.log[i]["network_fault_counts"] = json!(counts);
                                }
                                self.applied(
                                    i,
                                    now,
                                    format!("handle={handle}; prior policy revealed"),
                                );
                                self.changed_network.push(i);
                                continue;
                            }
                            let Some(pairs) = self.handles.remove(&handle) else {
                                self.fail(i, "heal of inactive handle".into());
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
                        action @ (Action::Pause { .. }
                        | Action::Resume { .. }
                        | Action::Crash { .. }
                        | Action::Reboot { .. }) => {
                            let op = lifecycle(&action).unwrap();
                            let m = self.entries[i].target.unwrap();
                            let life = self.life[m];
                            let to = match (op, life) {
                                (Lifecycle::Pause, Life::Up) => Life::Paused,
                                (Lifecycle::Resume, Life::Paused) => Life::Up,
                                (Lifecycle::Crash, Life::Up | Life::Paused) => Life::Down,
                                (Lifecycle::Reboot, Life::Down) => Life::Up,
                                _ => {
                                    self.fail(
                                        i,
                                        format!(
                                            "{} of a {} machine",
                                            action.name(),
                                            life_name(life)
                                        ),
                                    );
                                    continue;
                                }
                            };
                            self.life[m] = to;
                            if op == Lifecycle::Crash {
                                self.machine_down(m, now);
                            }
                            self.applied(i, now, format!("machine {}", life_name(to)));
                            self.lifecycle.push((m, op));
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
            self.stopped[machine] = matches!(self.entries[i].plan.action, Action::Stop { .. });
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

fn lifecycle(action: &Action) -> Option<Lifecycle> {
    match action {
        Action::Pause { .. } => Some(Lifecycle::Pause),
        Action::Resume { .. } => Some(Lifecycle::Resume),
        Action::Crash { .. } => Some(Lifecycle::Crash),
        Action::Reboot { .. } => Some(Lifecycle::Reboot),
        _ => None,
    }
}

fn life_name(life: Life) -> &'static str {
    match life {
        Life::Up => "running",
        Life::Paused => "paused",
        Life::Down => "down",
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;
    fn shape(id: u32, handle: &str, latency: u64) -> Value {
        json!({"id":id,"at_ns":id as u64*10,"deadline_ns":id as u64*10+1,"action":"shape","handle":handle,"from":["a"],"to":["b"],"latency_ns":latency,"jitter_ns":0,"loss_ppm":0})
    }
    fn heal(id: u32, handle: &str) -> Value {
        json!({"id":id,"at_ns":id as u64*10,"deadline_ns":id as u64*10+1,"action":"heal","handle":handle})
    }
    fn fixture(plans: Vec<Value>) -> Faults {
        let names = vec!["a".into(), "b".into(), "c".into()];
        let links = Links::new(
            1009,
            &names,
            Policy {
                latency_ns: 10,
                jitter_ns: 0,
                loss_ppm: 0,
                bandwidth_bps: None,
                queue_bound_ns: None,
            },
            BTreeMap::from([(
                (0, 1),
                Policy {
                    latency_ns: 20,
                    jitter_ns: 0,
                    loss_ppm: 0,
                    bandwidth_bps: None,
                    queue_bound_ns: None,
                },
            )]),
        )
        .unwrap();
        Faults::with_links(plans, names, links).unwrap()
    }
    #[test]
    fn overlapping_layers_heal_in_either_order_and_reveal_baseline_and_global() {
        for order in [["old", "new"], ["new", "old"]] {
            let mut f = fixture(vec![
                shape(1, "old", 30),
                shape(2, "new", 40),
                heal(3, order[0]),
                heal(4, order[1]),
            ]);
            f.apply_due(9);
            assert_eq!(f.links.policy((0, 1)).latency_ns, 20);
            f.apply_due(10);
            assert_eq!(f.links.policy((0, 1)).latency_ns, 30);
            f.apply_due(20);
            assert_eq!(f.links.policy((0, 1)).latency_ns, 40);
            assert_eq!(f.active_handles(), vec!["new", "old"]);
            f.apply_due(30);
            assert_eq!(
                f.links.policy((0, 1)).latency_ns,
                if order[0] == "old" { 40 } else { 30 }
            );
            f.apply_due(40);
            assert_eq!(f.links.policy((0, 1)).latency_ns, 20);
            assert_eq!(f.links.policy((1, 0)).latency_ns, 10);
            assert!(f.active_handles().is_empty());
            assert!(f.error.is_none());
            assert_eq!(f.log[0]["status"], "applied");
            assert!(f.log[0]["detail"].as_str().unwrap().contains("handle=old"));
        }
    }
    #[test]
    fn cut_dominates_live_shape_then_heal_resumes_identical_stream_position() {
        let mut f = fixture(vec![
            shape(1, "s", 30),
            json!({"id":2,"at_ns":20,"deadline_ns":21,"action":"partition","handle":"p","left":["a"],"right":["b"]}),
            heal(3, "p"),
            heal(4, "s"),
        ]);
        f.apply_due(10);
        let mut reference = f.links.clone();
        f.apply_due(20);
        assert_eq!(
            f.links.decide(0, 1, 20, 1500, f.blocked(0, 1)),
            crate::links::Outcome::Cut
        );
        assert_eq!(f.links.last, reference.last);
        f.apply_due(30);
        assert_eq!(
            f.links.decide(0, 1, 30, 1500, f.blocked(0, 1)),
            reference.decide(0, 1, 30, 1500, false)
        );
        let mut h = Sha256::new();
        f.links.witness(&mut h);
        let mut r = Sha256::new();
        reference.witness(&mut r);
        assert_eq!(h.finalize(), r.finalize());
        f.apply_due(40);
        assert_eq!(f.links.policy((0, 1)).latency_ns, 20);
    }
    #[test]
    fn invalid_shape_plans_and_inactive_heal_fail() {
        for invalid in [
            shape(1, "s", 9),
            json!({"id":1,"at_ns":10,"deadline_ns":11,"action":"shape","handle":"s","from":["a"],"to":["a"],"latency_ns":10,"jitter_ns":0,"loss_ppm":0}),
        ] {
            let mut f = fixture(vec![]);
            assert!(f.append(serde_json::from_value(invalid).unwrap()).is_err());
        }
        let mut f = fixture(vec![heal(1, "missing")]);
        f.apply_due(10);
        assert!(f.error.is_some());
    }
}
