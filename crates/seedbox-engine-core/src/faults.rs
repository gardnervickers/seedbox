//! Typed portable fault state. Only outstanding boundaries enter the scheduler;
//! completed history is retained as evidence but never searched to find work.
use crate::control::disjoint_sides;
use crate::links::{Links, Policy, QueueCounts};
use crate::network_faults::Counts;
use seedbox_control::{Action, NetworkFaults, Plan};
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Scheduled,
    Waiting,
    Applied(u64),
    Failed,
    Unavailable(u64),
}
pub use seedbox_engine_protocol::report::RowStatus;
/// Selected-connection evidence, fields in sorted key order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct SelectionCounts {
    pub dropped: u64,
    pub matched: u64,
}
/// One fault's evidence row, as reports and witnesses carry it. Fields are in
/// sorted key order and nested counts serialize with sorted keys, so a row
/// serializes byte for byte as the sorted-key JSON object it replaced.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct FaultRow {
    pub action: std::borrow::Cow<'static, str>,
    pub applied_at_ns: Option<u64>,
    pub detail: String,
    pub id: u32,
    pub machine: Option<String>,
    /// Nominal network counters when a network change applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_counters: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "sorted")]
    pub network_fault_counts: Option<Counts>,
    /// The wall-clock step asked of the guest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_ns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "sorted")]
    pub queue_counts: Option<QueueCounts>,
    /// The clock rate error asked of the guest, in parts per billion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_ppb: Option<i64>,
    /// The plan as requested, a sorted-key object.
    pub request: Value,
    pub requested_at_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_counts: Option<SelectionCounts>,
    pub status: RowStatus,
}
impl FaultRow {
    /// The row as a JSON value.
    pub fn json(&self) -> Value {
        serde_json::to_value(self).expect("a fault row is plain JSON")
    }
}
/// Serialize through a JSON value, whose object keys are sorted.
fn sorted<T: Serialize, S: Serializer>(value: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
    serde_json::to_value(value)
        .map_err(serde::ser::Error::custom)?
        .serialize(s)
}
/// A guest effect that matched no waiting fault, kept as evidence. Fields are
/// in sorted key order.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct RejectedEffect {
    pub at_ns: u64,
    pub detail: std::borrow::Cow<'static, str>,
    pub id: u32,
    pub machine: Option<String>,
    /// Always `"rejected"`.
    pub status: std::borrow::Cow<'static, str>,
    pub success: bool,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Entry {
    plan: Plan,
    target: Option<usize>,
    pairs: BTreeSet<(usize, usize)>,
    status: Status,
    dispatched: bool,
    dispatch_at: Option<u64>,
    /// The clock change this entry asks its target guest to make.
    clock: Option<ClockChange>,
}
/// A guest clock change: a wall-clock step in nanoseconds, or the absolute
/// rate error of the system clock in parts per billion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ClockChange {
    Step(i64),
    Rate(i64),
}
/// A clock skew or drift handle: the guest has been asked to make its change,
/// has done so, is undoing it, or lost it in a crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ClockState {
    Pending,
    Active,
    Healing,
    Lost,
}
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct Clock {
    machine: usize,
    change: ClockChange,
    state: ClockState,
}
impl Clock {
    fn unsettled(&self) -> &'static str {
        match self.change {
            ClockChange::Step(_) => "clock skew is not settled",
            ClockChange::Rate(_) => "clock drift is not settled",
        }
    }
}
/// Whole-machine lifecycle. A paused machine keeps its VM and executes
/// nothing; a down machine has no VM until it reboots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Life {
    Up,
    Paused,
    Down,
}
/// A lifecycle transition applied at a fault boundary, for the world to carry
/// out on the machine before anything else happens at that time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Lifecycle {
    Pause,
    Resume,
    Crash,
    Reboot,
    /// Charge CPU work at this many thousandths of the seeded speed.
    Throttle(u64),
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
enum Event {
    Apply,
    Dispatch,
    Deadline,
}
/// Commands for target guests: (machine, management line).
type Commands = Vec<(usize, Vec<u8>)>;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Faults {
    /// One evidence row per fault, by ID.
    pub log: Vec<FaultRow>,
    pub links: Links,
    pub connections: crate::connections::Connections,
    entries: Vec<Entry>,
    boundaries: BTreeSet<(u64, u32, Event)>,
    waiting_ready: Vec<BTreeSet<u32>>,
    ready: Vec<bool>,
    stopped: Vec<bool>,
    /// The application's process group is SIGSTOPped; its machine runs on.
    frozen: Vec<bool>,
    life: Vec<Life>,
    /// Outstanding lifecycle requests per machine, by time: nothing may be
    /// delivered early to a machine across one of these.
    lifecycle_at: Vec<BTreeSet<(u64, u32)>>,
    lifecycle: Vec<(usize, Lifecycle)>,
    pub readiness: Vec<Value>,
    handles: BTreeMap<String, BTreeSet<(usize, usize)>>,
    clocks: BTreeMap<String, Clock>,
    /// Active throttle handles: machine and factor in thousandths.
    throttles: BTreeMap<String, (usize, u64)>,
    #[serde(with = "crate::checkpoint::ordered_map")]
    cuts: BTreeMap<(usize, usize), usize>,
    names: Vec<String>,
    changed_network: Vec<usize>,
    pub error: Option<String>,
    pub rejected_effects: Vec<RejectedEffect>,
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
    /// [`Faults::with_plans`] for plans held as JSON.
    pub fn with_links(plans: Vec<Value>, names: Vec<String>, links: Links) -> Result<Self, String> {
        let plans = plans
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<Plan>, _>>()
            .map_err(|e| format!("invalid fault plan: {e}"))?;
        Self::with_plans(plans, names, links)
    }
    pub fn with_plans(plans: Vec<Plan>, names: Vec<String>, links: Links) -> Result<Self, String> {
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
            frozen: vec![false; count],
            life: vec![Life::Up; count],
            lifecycle_at: vec![BTreeSet::new(); count],
            lifecycle: vec![],
            readiness: vec![],
            handles: BTreeMap::new(),
            clocks: BTreeMap::new(),
            throttles: BTreeMap::new(),
            cuts: BTreeMap::new(),
            names,
            changed_network: vec![],
            error: None,
            rejected_effects: vec![],
        };
        for (i, plan) in plans.into_iter().enumerate() {
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
    fn machine_index(&self, name: &str, unknown: &str) -> Result<usize, String> {
        self.names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| unknown.to_owned())
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
            | Action::PauseProcess { machine }
            | Action::ResumeProcess { machine }
            | Action::Pause { machine }
            | Action::Resume { machine }
            | Action::Crash { machine }
            | Action::Reboot { machine }
            | Action::Throttle { machine, .. } => {
                if let Action::Throttle {
                    handle,
                    factor_permille,
                    ..
                } = &plan.action
                {
                    if handle.is_empty()
                        || !seedbox_control::THROTTLE_PERMILLE.contains(factor_permille)
                    {
                        return Err("invalid throttle handle/factor".into());
                    }
                }
                target = Some(self.machine_index(machine, "unknown fault machine")?);
            }
            Action::Partition {
                handle,
                left,
                right,
            } => {
                if handle.is_empty() || left.is_empty() || right.is_empty() {
                    return Err("empty fault handle/partition side".into());
                }
                let [left, right] = disjoint_sides(
                    [left, right],
                    |n| self.machine_index(n, "unknown partition machine"),
                    "invalid/disjoint fault partition members",
                )?;
                for &a in &left {
                    for &b in &right {
                        pairs.insert((a, b));
                        pairs.insert((b, a));
                    }
                }
            }
            Action::Shape {
                handle, from, to, ..
            } => {
                let (policy, network_faults) = shape(&plan.action).expect("a shape action");
                policy.validate_shape(self.links.global.latency_ns, &network_faults)?;
                if handle.is_empty() || from.is_empty() || to.is_empty() {
                    return Err("empty fault handle/shape side".into());
                }
                let [from, to] = disjoint_sides(
                    [from, to],
                    |n| self.machine_index(n, "unknown shape machine"),
                    "shape sides must be disjoint with unique machines",
                )?;
                for &a in &from {
                    for &b in &to {
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
            Action::ClockSkew {
                handle,
                machine,
                offset_ns,
                max_offset_ns,
            } => {
                seedbox_control::validate_clock_skew(*offset_ns, *max_offset_ns)?;
                if handle.is_empty() {
                    return Err("empty fault handle".into());
                }
                target = Some(self.machine_index(machine, "unknown fault machine")?);
            }
            Action::ClockDrift {
                handle,
                machine,
                rate_ppb,
                max_rate_ppb,
            } => {
                seedbox_control::validate_clock_drift(*rate_ppb, *max_rate_ppb)?;
                if handle.is_empty() {
                    return Err("empty fault handle".into());
                }
                target = Some(
                    self.names
                        .iter()
                        .position(|n| n == machine)
                        .ok_or("unknown fault machine")?,
                );
            }
        }
        Ok(Entry {
            plan,
            target,
            pairs,
            status: Status::Scheduled,
            dispatched: false,
            dispatch_at: None,
            clock: None,
        })
    }
    /// Load a plan before execution. IDs follow admission order; scheduled
    /// times may differ, and the boundary queue defines execution order.
    pub fn append(&mut self, plan: Plan) -> Result<u32, String> {
        self.admit(plan, 0)
    }

    /// Admit a runtime plan without allowing an effect in the committed past.
    /// Equal-time plans execute in their stable admission-ID order.
    pub fn admit(&mut self, plan: Plan, boundary: u64) -> Result<u32, String> {
        let entry = self.prepare(plan, boundary)?;
        Ok(self.commit(entry))
    }

    /// Validate without changing fault state, assigning the next admission ID.
    /// The caller can retain this exact plan before committing it.
    pub fn propose(&self, plan: Plan, boundary: u64) -> Result<Plan, String> {
        Ok(self.prepare(plan, boundary)?.plan)
    }

    /// Admit a retained plan without silently replacing its identity. A changed
    /// scenario/guest admission sequence is a replay divergence, not a new ID.
    pub fn admit_exact(&mut self, plan: Plan, boundary: u64) -> Result<u32, String> {
        if plan.id != self.next_id()? {
            return Err("retained fault ID differs from the next admission ID".into());
        }
        self.admit(plan, boundary)
    }

    fn next_id(&self) -> Result<u32, String> {
        u32::try_from(self.entries.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| "fault ID exhausted".into())
    }

    fn prepare(&self, mut plan: Plan, boundary: u64) -> Result<Entry, String> {
        if plan.at_ns < boundary {
            return Err("fault time precedes admission boundary".into());
        }
        plan.id = self.next_id()?;
        self.compile(plan)
    }

    fn commit(&mut self, entry: Entry) -> u32 {
        let id = entry.plan.id;
        self.log.push(FaultRow {
            action: entry.plan.action.name().into(),
            applied_at_ns: None,
            detail: "scheduled".into(),
            id,
            machine: entry.plan.action.machine().map(str::to_owned),
            network_counters: None,
            network_fault_counts: None,
            offset_ns: None,
            queue_counts: None,
            rate_ppb: None,
            request: json!(entry.plan),
            requested_at_ns: entry.plan.at_ns,
            selection_counts: None,
            status: RowStatus::Unrequested,
        });
        self.boundaries.insert((entry.plan.at_ns, id, Event::Apply));
        if let (Some(m), Some(_)) = (entry.target, lifecycle(&entry.plan.action)) {
            self.lifecycle_at[m].insert((entry.plan.at_ns, id));
        }
        self.entries.push(entry);
        id
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
    /// Machine `m`'s lifecycle as the fault model sees it.
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
    fn index(&self, id: u32) -> Option<usize> {
        id.checked_sub(1)
            .map(|n| n as usize)
            .filter(|&i| i < self.entries.len())
    }
    pub fn plan(&self, id: u32) -> Option<&Plan> {
        self.index(id).map(|i| &self.entries[i].plan)
    }
    /// Fault `id`'s evidence row.
    pub fn row(&self, id: u32) -> Option<&FaultRow> {
        self.index(id).map(|i| &self.log[i])
    }
    pub fn outcome(&self, id: u32) -> Option<(Status, &str)> {
        self.index(id)
            .map(|i| (self.entries[i].status, self.log[i].detail.as_str()))
    }
    /// Record the nominal network counters as fault row `i`'s network change applied.
    pub fn set_network_counters(&mut self, i: usize, counters: Value) {
        self.log[i].network_counters = Some(counters);
    }
    pub fn active_handles(&self) -> Vec<String> {
        let mut handles: Vec<_> = self
            .handles
            .keys()
            .cloned()
            .chain(self.links.layers.values().map(|l| l.handle.clone()))
            .collect();
        handles.extend(self.connections.selections.keys().cloned());
        handles.extend(self.clocks.keys().cloned());
        handles.extend(self.throttles.keys().cloned());
        handles.sort();
        handles
    }
    /// The factor of machine `m`'s active throttle, if any. A rebooted
    /// incarnation takes it on again.
    pub fn throttle(&self, m: usize) -> Option<u64> {
        self.throttles
            .values()
            .find(|(machine, _)| *machine == m)
            .map(|(_, factor)| *factor)
    }
    pub fn process_state(&self, machine: usize) -> &'static str {
        match self.life[machine] {
            Life::Paused => "Paused",
            Life::Down => "Down",
            Life::Up if self.stopped[machine] => "Stopped",
            Life::Up if self.frozen[machine] => "Frozen",
            Life::Up => "Running",
        }
    }
    pub fn admission_error(&self, action: &Action) -> Option<&'static str> {
        if let Action::Heal { handle } = action {
            let clock = self.clocks.get(handle)?;
            return match clock.state {
                ClockState::Pending | ClockState::Healing => Some(clock.unsettled()),
                ClockState::Active if self.life[clock.machine] == Life::Paused => {
                    Some("target machine is paused")
                }
                _ => None,
            };
        }
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
            action if action.is_process() && self.life[index] == Life::Down => None,
            // A paused guest cannot acknowledge; its deadline would fail the run.
            action if action.is_process() && self.life[index] == Life::Paused => {
                Some("target machine is paused")
            }
            Action::Stop { .. } if self.stopped[index] => Some("target process is already stopped"),
            Action::Restart { .. } if !self.stopped[index] => {
                Some("restart requires a stopped target process")
            }
            Action::PauseProcess { .. } if self.stopped[index] => Some("target process is stopped"),
            Action::PauseProcess { .. } if self.frozen[index] => {
                Some("target process is already paused")
            }
            Action::ResumeProcess { .. } if !self.frozen[index] => {
                Some("target process is not paused")
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
            Action::ClockSkew { .. } | Action::ClockDrift { .. }
                if self.life[index] == Life::Down =>
            {
                Some("target machine is down")
            }
            Action::ClockSkew { .. } | Action::ClockDrift { .. }
                if self.life[index] == Life::Paused =>
            {
                Some("target machine is paused")
            }
            Action::ClockDrift { .. } if self.drifting(index) => {
                Some("target machine clock is already drifting")
            }
            Action::Throttle { .. } if self.throttle(index).is_some() => {
                Some("target machine is already throttled")
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
        let log = &mut self.log;
        let drop = self.connections.drops(c, cut, |s| {
            log[s.fault_id as usize - 1].selection_counts = Some(SelectionCounts {
                dropped: s.dropped,
                matched: s.matched,
            });
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
        // An injected reset is observed like the original segment would be.
        if deliveries[0].1 != frame && crate::connections::is_reset_frame(&deliveries[0].1) {
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
            let row = &mut self.log[id as usize - 1];
            if layer.policy.bandwidth_bps.is_some() {
                row.queue_counts = Some(layer.queue_counts);
            }
            if !layer.network_faults.is_off() {
                row.network_fault_counts = Some(layer.network_counts);
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
    /// Resolve entry `i`: nothing of it remains outstanding.
    fn settle(&mut self, i: usize, status: Status, detail: String) {
        self.clear_pending(i);
        self.entries[i].status = status;
        let row = &mut self.log[i];
        (row.status, row.applied_at_ns) = match status {
            Status::Applied(time) => (RowStatus::Applied, Some(time)),
            Status::Failed => (RowStatus::Failed, None),
            Status::Unavailable(_) => (RowStatus::Unavailable, None),
            Status::Scheduled | Status::Waiting => unreachable!("settled faults are resolved"),
        };
        row.detail = detail;
    }
    fn fail(&mut self, i: usize, message: String) {
        self.settle(i, Status::Failed, message.clone());
        self.error = Some(message);
    }
    fn applied(&mut self, i: usize, time: u64, detail: String) {
        self.settle(i, Status::Applied(time), detail);
    }
    fn unavailable(&mut self, i: usize, time: u64, detail: &str) {
        self.settle(i, Status::Unavailable(time), detail.into());
    }
    /// Resolve every outstanding effect for an exited VM, independently of
    /// whether the fault came from a static plan, trigger, or runtime request.
    pub fn machine_exited(&mut self, machine: usize, time: u64) {
        for index in 0..self.entries.len() {
            let id = self.entries[index].plan.id;
            self.target_exited(id, machine, time);
        }
    }

    /// A target VM that exits before acknowledging leaves its process fault unavailable, not failed.
    pub fn target_exited(&mut self, id: u32, machine: usize, time: u64) {
        let Some(i) = self.index(id) else {
            return;
        };
        if self.entries[i].status != Status::Waiting
            || self.entries[i].target != Some(machine)
            || time < self.entries[i].plan.at_ns
            || time > self.entries[i].plan.deadline_ns
        {
            return;
        }
        self.unavailable(i, time, "target VM exited before the process effect");
        self.forget_clocks(machine);
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
        self.forget_clocks(m);
        self.ready[m] = false;
        self.stopped[m] = false;
        self.frozen[m] = false;
    }
    /// A new incarnation boots from the recorded wall clock at the nominal rate,
    /// so no skew or drift survives on machine `m`. Settled handles stay until
    /// healed; unsettled ones were resolved as unavailable with their entries.
    fn forget_clocks(&mut self, m: usize) {
        self.clocks.retain(|_, c| {
            c.machine != m || c.state == ClockState::Active || c.state == ClockState::Lost
        });
        for c in self.clocks.values_mut().filter(|c| c.machine == m) {
            c.state = ClockState::Lost;
        }
    }
    /// Whether machine `m` has a drift that has not ended with its incarnation;
    /// a machine runs at one rate, so drifts do not stack.
    fn drifting(&self, m: usize) -> bool {
        self.clocks.values().any(|c| {
            c.machine == m
                && matches!(c.change, ClockChange::Rate(_))
                && c.state != ClockState::Lost
        })
    }
    /// The management command that asks entry `i`'s target guest for its effect.
    fn management_command(&self, i: usize) -> Vec<u8> {
        let e = &self.entries[i];
        let id = e.plan.id;
        match e.clock {
            Some(ClockChange::Step(step)) => format!("clock_step {id} {step}\n"),
            Some(ClockChange::Rate(rate)) => format!("clock_rate {id} {rate}\n"),
            None => format!("{} {id}\n", command(&e.plan.action)),
        }
        .into_bytes()
    }
    /// Wait for entry `i`'s guest effect: arm its deadline and dispatch it now
    /// if the target is ready, or at readiness otherwise.
    fn request_effect(&mut self, i: usize, commands: &mut Commands) {
        let id = self.entries[i].plan.id;
        self.boundaries
            .insert((self.entries[i].plan.deadline_ns, id, Event::Deadline));
        if let Some(m) = self.entries[i].target {
            if self.ready[m] {
                self.entries[i].dispatched = true;
                commands.push((m, self.management_command(i)));
            } else {
                self.waiting_ready[m].insert(id);
            }
        }
    }
    /// Readiness is the reserved success id0; process commands wait for it.
    /// Historical readiness survives crashes and prevents a reboot from
    /// changing the initial boot/execution stage back to booting.
    pub(crate) fn initial_applications_ready(&self) -> bool {
        self.names.iter().all(|name| {
            self.readiness
                .iter()
                .any(|row| row.get("machine").and_then(Value::as_str) == Some(name.as_str()))
        })
    }

    pub fn management_ready(&mut self, m: usize, time: u64, latency: u64) -> Result<(), String> {
        if m >= self.names.len() || self.ready[m] {
            return Err("duplicate/unknown management readiness".into());
        }
        self.ready[m] = true;
        self.readiness
            .push(json!({ "machine": self.names[m], "at_ns": time }));
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
    /// Apply every boundary at or before `now`, in (time, ID, event) order.
    /// Returns the management commands to send, in order.
    pub fn apply_due(&mut self, now: u64) -> Commands {
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
                            commands.push((m, self.management_command(i)));
                        }
                    }
                }
                Event::Apply => self.apply(i, now, &mut commands),
            }
        }
        commands
    }
    /// Entry `i`'s time has come: carry out its action.
    fn apply(&mut self, i: usize, now: u64, commands: &mut Commands) {
        self.entries[i].status = Status::Waiting;
        self.log[i].status = RowStatus::Requested;
        match self.entries[i].plan.action.clone() {
            action
                if action.is_process()
                    && self.entries[i]
                        .target
                        .is_some_and(|m| self.life[m] == Life::Down) =>
            {
                self.unavailable(i, now, "target machine is down");
            }
            Action::Stop { .. }
            | Action::Restart { .. }
            | Action::PauseProcess { .. }
            | Action::ResumeProcess { .. } => {
                self.request_effect(i, commands);
            }
            Action::ClockSkew {
                handle,
                offset_ns,
                max_offset_ns,
                ..
            } => self.apply_clock_skew(i, handle, offset_ns, max_offset_ns, commands),
            Action::ClockDrift {
                handle,
                rate_ppb,
                max_rate_ppb,
                ..
            } => self.apply_clock_drift(i, handle, rate_ppb, max_rate_ppb, commands),
            Action::Partition { handle, .. } => self.apply_partition(i, now, handle),
            action @ Action::Shape { .. } => self.apply_shape(i, now, &action),
            Action::DropConnection { handle, connection } => {
                self.apply_drop_connection(i, now, handle, connection)
            }
            Action::Heal { handle } if self.clocks.contains_key(&handle) => {
                self.heal_clock(i, now, handle, commands)
            }
            Action::Throttle {
                handle,
                factor_permille,
                ..
            } => self.apply_throttle(i, now, handle, factor_permille),
            Action::Heal { handle } => self.heal(i, now, handle),
            action @ (Action::Pause { .. }
            | Action::Resume { .. }
            | Action::Crash { .. }
            | Action::Reboot { .. }) => self.apply_lifecycle(i, now, &action),
        }
    }
    fn apply_clock_skew(
        &mut self,
        i: usize,
        handle: String,
        offset_ns: Option<i64>,
        max_offset_ns: Option<u64>,
        commands: &mut Commands,
    ) {
        if self.active_handles().contains(&handle) {
            return self.fail(i, "duplicate active handle".into());
        }
        let m = self.entries[i].target.expect("a clock skew has a target");
        if self.life[m] == Life::Down {
            return self.fail(i, "clock_skew of a down machine".into());
        }
        // Draw only for a seeded skew, at this boundary.
        let offset = match (offset_ns, max_offset_ns) {
            (Some(offset), _) => offset,
            (None, max) => match self.links.clock_offset(m, max.unwrap_or(0)) {
                Ok(offset) => offset,
                Err(error) => return self.fail(i, error),
            },
        };
        self.log[i].offset_ns = Some(offset);
        self.clocks.insert(
            handle,
            Clock {
                machine: m,
                change: ClockChange::Step(offset),
                state: ClockState::Pending,
            },
        );
        self.entries[i].clock = Some(ClockChange::Step(offset));
        self.request_effect(i, commands);
    }
    fn apply_clock_drift(
        &mut self,
        i: usize,
        handle: String,
        rate_ppb: Option<i64>,
        max_rate_ppb: Option<u64>,
        commands: &mut Commands,
    ) {
        if self.active_handles().contains(&handle) {
            return self.fail(i, "duplicate active handle".into());
        }
        let m = self.entries[i].target.expect("a clock drift has a target");
        if self.life[m] == Life::Down {
            return self.fail(i, "clock_drift of a down machine".into());
        }
        if self.drifting(m) {
            return self.fail(i, "clock_drift of an already drifting machine".into());
        }
        // Draw only for a seeded drift, at this boundary.
        let rate = match (rate_ppb, max_rate_ppb) {
            (Some(rate), _) => rate,
            (None, max) => match self.links.clock_offset(m, max.unwrap_or(0)) {
                Ok(rate) => rate,
                Err(error) => return self.fail(i, error),
            },
        };
        self.log[i].rate_ppb = Some(rate);
        self.clocks.insert(
            handle,
            Clock {
                machine: m,
                change: ClockChange::Rate(rate),
                state: ClockState::Pending,
            },
        );
        self.entries[i].clock = Some(ClockChange::Rate(rate));
        self.request_effect(i, commands);
    }
    fn apply_partition(&mut self, i: usize, now: u64, handle: String) {
        if self.active_handles().contains(&handle) {
            return self.fail(i, "duplicate active partition".into());
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
    fn apply_shape(&mut self, i: usize, now: u64, action: &Action) {
        let Action::Shape { handle, .. } = action else {
            unreachable!("a shape action")
        };
        let (policy, network_faults) = shape(action).expect("a shape action");
        if self.active_handles().contains(handle) {
            return self.fail(i, "duplicate active handle".into());
        }
        let id = self.entries[i].plan.id;
        let pairs = self.entries[i].pairs.clone();
        if let Err(error) =
            self.links
                .shape_with_faults(id, handle.clone(), pairs, policy, network_faults)
        {
            return self.fail(i, error);
        }
        if policy.bandwidth_bps.is_some() {
            self.log[i].queue_counts = Some(QueueCounts::default());
        }
        if !network_faults.is_off() {
            self.log[i].network_fault_counts = Some(Counts::default());
        }
        self.applied(
            i,
            now,
            format!("handle={handle}; in-flight frames retained"),
        );
        self.changed_network.push(i);
    }
    fn apply_drop_connection(
        &mut self,
        i: usize,
        now: u64,
        handle: String,
        connection: seedbox_control::Connection,
    ) {
        if self.active_handles().contains(&handle) {
            return self.fail(i, "duplicate active handle".into());
        }
        let id = self.entries[i].plan.id;
        if let Err(error) = self
            .connections
            .select(id, handle.clone(), connection, &self.names)
        {
            return self.fail(i, error);
        }
        self.log[i].selection_counts = Some(SelectionCounts::default());
        self.applied(
            i,
            now,
            format!("handle={handle}; armed; in-flight frames retained"),
        );
    }
    fn heal_clock(&mut self, i: usize, now: u64, handle: String, commands: &mut Commands) {
        let clock = self.clocks[&handle];
        match clock.state {
            ClockState::Pending | ClockState::Healing => {
                self.fail(i, clock.unsettled().into());
            }
            ClockState::Lost => {
                self.clocks.remove(&handle);
                let what = match clock.change {
                    ClockChange::Step(_) => "skew",
                    ClockChange::Rate(_) => "drift",
                };
                self.applied(
                    i,
                    now,
                    format!("handle={handle}; {what} ended with its machine incarnation"),
                );
            }
            ClockState::Active => {
                if let Some(c) = self.clocks.get_mut(&handle) {
                    c.state = ClockState::Healing;
                }
                // Undo the change: step back, or restore the nominal rate.
                let undo = match clock.change {
                    ClockChange::Step(offset) => {
                        self.log[i].offset_ns = Some(-offset);
                        ClockChange::Step(-offset)
                    }
                    ClockChange::Rate(_) => {
                        self.log[i].rate_ppb = Some(0);
                        ClockChange::Rate(0)
                    }
                };
                self.entries[i].target = Some(clock.machine);
                self.entries[i].clock = Some(undo);
                self.request_effect(i, commands);
            }
        }
    }
    fn apply_throttle(&mut self, i: usize, now: u64, handle: String, factor_permille: u64) {
        let m = self.entries[i].target.expect("a throttle has a target");
        if self.active_handles().contains(&handle) {
            return self.fail(i, "duplicate active handle".into());
        }
        if self.throttle(m).is_some() {
            return self.fail(i, "target machine is already throttled".into());
        }
        self.throttles.insert(handle.clone(), (m, factor_permille));
        self.applied(
            i,
            now,
            format!("handle={handle}; factor_permille={factor_permille}"),
        );
        self.lifecycle
            .push((m, Lifecycle::Throttle(factor_permille)));
    }
    /// Heal a throttle, connection selector, shape layer or partition.
    fn heal(&mut self, i: usize, now: u64, handle: String) {
        if let Some((m, _)) = self.throttles.remove(&handle) {
            self.applied(i, now, format!("handle={handle}; seeded speed restored"));
            self.lifecycle.push((m, Lifecycle::Throttle(1_000)));
            return;
        }
        if let Some(selection) = self.connections.selections.remove(&handle) {
            self.log[i].selection_counts = Some(SelectionCounts {
                dropped: selection.dropped,
                matched: selection.matched,
            });
            self.applied(
                i,
                now,
                format!(
                    "handle={handle}; selected packets dropped={}",
                    selection.dropped
                ),
            );
            return;
        }
        let layers = || self.links.layers.values().filter(|l| l.handle == handle);
        let queue_counts = layers()
            .find(|l| l.policy.bandwidth_bps.is_some())
            .map(|l| l.queue_counts);
        let network_counts = layers()
            .find(|l| !l.network_faults.is_off())
            .map(|l| l.network_counts);
        if self.links.heal(&handle) {
            if queue_counts.is_some() {
                self.log[i].queue_counts = queue_counts;
            }
            if network_counts.is_some() {
                self.log[i].network_fault_counts = network_counts;
            }
            self.applied(i, now, format!("handle={handle}; prior policy revealed"));
            self.changed_network.push(i);
            return;
        }
        let Some(pairs) = self.handles.remove(&handle) else {
            return self.fail(i, "heal of inactive handle".into());
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
    fn apply_lifecycle(&mut self, i: usize, now: u64, action: &Action) {
        let op = lifecycle(action).expect("a lifecycle action");
        let m = self.entries[i]
            .target
            .expect("a lifecycle action has a target");
        let life = self.life[m];
        let to = match (op, life) {
            (Lifecycle::Pause, Life::Up) => Life::Paused,
            (Lifecycle::Resume, Life::Paused) => Life::Up,
            (Lifecycle::Crash, Life::Up | Life::Paused) => Life::Down,
            (Lifecycle::Reboot, Life::Down) => Life::Up,
            _ => {
                let error = format!("{} of a {} machine", action.name(), life_name(life));
                return self.fail(i, error);
            }
        };
        self.life[m] = to;
        if op == Lifecycle::Crash {
            self.machine_down(m, now);
        }
        self.applied(i, now, format!("machine {}", life_name(to)));
        self.lifecycle.push((m, op));
    }
    pub fn effect(&mut self, machine: usize, id: u32, time: u64, success: bool) {
        let rejected = |detail: &'static str| RejectedEffect {
            at_ns: time,
            detail: detail.into(),
            id,
            machine: self.names.get(machine).cloned(),
            status: "rejected".into(),
            success,
        };
        let Some(i) = self.index(id) else {
            self.rejected_effects.push(rejected("unknown request ID"));
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
            self.rejected_effects
                .push(rejected("state/machine/time mismatch"));
            self.fail(
                i,
                format!("stale/duplicate/miscorrelated fault effect {id}"),
            );
            return;
        }
        let clock_handle = match &self.entries[i].plan.action {
            Action::ClockSkew { handle, .. }
            | Action::ClockDrift { handle, .. }
            | Action::Heal { handle } => Some(handle.clone()),
            _ => None,
        };
        if let Some(handle) = clock_handle {
            let healing = matches!(self.entries[i].plan.action, Action::Heal { .. });
            if success {
                let detail = match self.entries[i].clock {
                    Some(ClockChange::Rate(rate)) => format!("guest clock rate set to {rate} ppb"),
                    Some(ClockChange::Step(step)) => {
                        format!("guest wall clock stepped by {step} ns")
                    }
                    None => "guest wall clock stepped by 0 ns".into(),
                };
                if healing {
                    self.clocks.remove(&handle);
                } else if let Some(clock) = self.clocks.get_mut(&handle) {
                    clock.state = ClockState::Active;
                }
                self.applied(i, time, format!("handle={handle}; {detail}"));
            } else {
                if healing {
                    if let Some(clock) = self.clocks.get_mut(&handle) {
                        clock.state = ClockState::Active;
                    }
                } else {
                    self.clocks.remove(&handle);
                }
                self.fail(i, format!("guest rejected fault {id}"));
            }
            return;
        }
        if success {
            match self.entries[i].plan.action {
                Action::Stop { .. } => {
                    self.stopped[machine] = true;
                    self.frozen[machine] = false;
                }
                Action::PauseProcess { .. } => self.frozen[machine] = true,
                Action::ResumeProcess { .. } => self.frozen[machine] = false,
                _ => self.stopped[machine] = false,
            }
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

/// A shape action's link policy and impairments.
fn shape(action: &Action) -> Option<(Policy, NetworkFaults)> {
    match action {
        Action::Shape {
            latency_ns,
            jitter_ns,
            loss_ppm,
            bandwidth_bps,
            queue_bound_ns,
            network_faults,
            ..
        } => Some((
            Policy {
                latency_ns: *latency_ns,
                jitter_ns: *jitter_ns,
                loss_ppm: *loss_ppm,
                bandwidth_bps: *bandwidth_bps,
                queue_bound_ns: *queue_bound_ns,
            },
            *network_faults,
        )),
        _ => None,
    }
}

/// The supervisor's management verb for a process fault.
fn command(action: &Action) -> &'static str {
    match action {
        Action::Restart { .. } => "start",
        Action::PauseProcess { .. } => "freeze",
        Action::ResumeProcess { .. } => "thaw",
        _ => "stop",
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
            assert_eq!(f.log[0].json()["status"], "applied");
            assert!(f.log[0].json()["detail"]
                .as_str()
                .unwrap()
                .contains("handle=old"));
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
    /// Fault rows are witnessed and reported as text: their bytes must equal
    /// the sorted-key JSON objects they replaced.
    #[test]
    fn fault_row_bytes_are_pinned() {
        let mut row = FaultRow {
            action: "shape".into(),
            applied_at_ns: Some(10),
            detail: "applied".into(),
            id: 2,
            machine: Some("a".into()),
            network_counters: Some(json!({"b": 1, "a": 2})),
            network_fault_counts: Some(Counts {
                matched: 1,
                duplicated: 2,
                ..Counts::default()
            }),
            offset_ns: Some(-5),
            queue_counts: Some(QueueCounts {
                matched: 3,
                queue_dropped: 1,
                delayed_max_ns: 9,
            }),
            rate_ppb: None,
            request: json!({"id": 2, "action": "shape"}),
            requested_at_ns: 7,
            selection_counts: Some(SelectionCounts {
                dropped: 1,
                matched: 4,
            }),
            status: RowStatus::Applied,
        };
        let text = serde_json::to_string(&row).unwrap();
        assert_eq!(text, serde_json::to_string(&row.json()).unwrap());
        assert_eq!(
            text,
            concat!(
                r#"{"action":"shape","applied_at_ns":10,"detail":"applied","id":2,"machine":"a","#,
                r#""network_counters":{"a":2,"b":1},"network_fault_counts":{"burst_dropped":0,"#,
                r#""delayed_max_ns":0,"duplicated":2,"matched":1,"reordered":0,"spiked":0,"#,
                r#""tcp_reset":0},"offset_ns":-5,"queue_counts":{"delayed_max_ns":9,"matched":3,"#,
                r#""queue_dropped":1},"request":{"action":"shape","id":2},"requested_at_ns":7,"#,
                r#""selection_counts":{"dropped":1,"matched":4},"status":"applied"}"#
            )
        );
        row.applied_at_ns = None;
        row.machine = None;
        row.network_counters = None;
        row.network_fault_counts = None;
        row.offset_ns = None;
        row.queue_counts = None;
        row.selection_counts = None;
        row.status = RowStatus::Unrequested;
        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            concat!(
                r#"{"action":"shape","applied_at_ns":null,"detail":"applied","id":2,"machine":null,"#,
                r#""request":{"action":"shape","id":2},"requested_at_ns":7,"status":"unrequested"}"#
            )
        );
    }
}

#[cfg(test)]
#[path = "clock_skew_tests.rs"]
mod clock_skew_tests;

#[cfg(test)]
#[path = "clock_drift_tests.rs"]
mod clock_drift_tests;

#[cfg(test)]
#[path = "faults_tests.rs"]
mod policy_tests;
