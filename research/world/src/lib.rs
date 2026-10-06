//! Plain serializable world data and reliable control protocol.
//! One caller thread advances events/VMs. Event ticks are branch-count-domain inputs;
//! native fixture delays and integer tick-to-Linux-ns policy are uncalibrated inputs.
//! Opaque Ethernet frames and block bytes; no TCP, filesystem, or Linux claim.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
mod protocol;
pub use protocol::*;
mod client;
pub use client::*;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MachineId(pub u32);
impl From<u32> for MachineId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);
impl RequestId {
    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }
}
pub type TimeTicks = u64;
pub type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub machine: MachineId,
    pub incarnation: u64,
    pub request: RequestId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Endpoint {
    Controller,
    Machine(MachineId),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    Machine(MachineId),
    Az(u32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultHandle(pub u64);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LossRule {
    pub from: Scope,
    pub to: Scope,
    pub bidirectional: bool,
    pub numerator: u64,
    pub denominator: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockOp {
    Read { offset: usize, len: usize },
    Write { offset: usize, data: Vec<u8> },
    WriteThrough { offset: usize, data: Vec<u8> },
    Flush,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionKind {
    Tx,
    Read(Vec<u8>),
    Write,
    Flush,
    Dma,
    Reply(Vec<u8>),
    Timeout,
    Obsolete,
    Slept,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    pub owner: Option<Owner>,
    pub kind: CompletionKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceivedFrame {
    pub id: RequestId,
    pub from: Endpoint,
    pub to: Endpoint,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize)]
pub struct Defects {
    pub erase_overlap: bool,
    pub ignore_owner: bool,
    pub early_flush: bool,
    pub cancel_wire: bool,
    pub cancel_media: bool,
    pub batch_events: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Machine {
    az: u32,
    incarnation: u64,
    alive: bool,
    volatile: Vec<u8>,
    durable: Vec<u8>,
    memory: Vec<u8>,
    next_submission: u64,
    pending_writes: BTreeMap<u64, TimeTicks>,
    inbox: Vec<ReceivedFrame>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Event {
    Wake(RequestId),
    Wire(ReceivedFrame),
    Callback {
        owner: Owner,
        kind: CompletionKind,
    },
    Block {
        owner: Owner,
        submission: u64,
        through: u64,
        op: BlockOp,
    },
    Dma {
        owner: Owner,
        offset: usize,
        data: Vec<u8>,
    },
    Media {
        owner: Owner,
        submission: u64,
        offset: usize,
        data: Vec<u8>,
    },
    Timeout(RequestId),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct World {
    schema_version: u32,
    clock_policy: ClockPolicy,
    channels: BTreeMap<ControllerId, ReliableChannel>,
    control_ingress: VecDeque<RequestId>,
    pending_commands: BTreeMap<RequestId, PendingCommand>,
    boundaries: BTreeMap<MachineId, StoppedGuestBoundary>,
    controller_boundaries: BTreeMap<ControllerId, StoppedGuestBoundary>,
    seed: u64,
    now: TimeTicks,
    sequence: u64,
    next_id: u64,
    machines: BTreeMap<MachineId, Machine>,
    faults: BTreeMap<u64, LossRule>,
    #[serde(with = "event_queue")]
    queue: BTreeMap<(TimeTicks, u64), Event>,
    completions: BTreeMap<RequestId, Completion>,
    pending_queries: BTreeSet<RequestId>,
    streams: BTreeMap<String, u64>,
    delivered: BTreeMap<RequestId, ReceivedFrame>,
    drops: BTreeMap<u64, u64>,
    backend_state: BTreeMap<String, Value>,
    controller_inbox: Vec<ReceivedFrame>,
    stale: u64,
    defects: Defects,
    #[serde(skip, default)]
    trace: Vec<Value>,
}
impl World {
    pub fn snapshot(&self) -> Value {
        serde_json::to_value(self).expect("serializable world data")
    }
    fn emit(&mut self, kind: &str, detail: Value) {
        self.trace
            .push(json!({"tick":self.now,"event":kind,"detail":detail,"state":self.snapshot()}));
    }
    fn take_completion(&mut self, id: RequestId) -> Option<Completion> {
        let mut c = self.completions.remove(&id)?;
        if let Some(owner) = c.owner
            && !self.owns(owner)
            && c.kind != CompletionKind::Obsolete
        {
            c.kind = CompletionKind::Obsolete;
            self.stale += 1;
            self.emit("obsolete_completed_response", json!({"owner":owner}));
        }
        Some(c)
    }
    fn id(&mut self) -> RequestId {
        self.next_id = self.next_id.checked_add(1).expect("id overflow");
        RequestId(self.next_id)
    }
    fn schedule(&mut self, delay: TimeTicks, event: Event) -> (TimeTicks, u64) {
        let at = self.now.checked_add(delay).expect("time overflow");
        self.sequence = self.sequence.checked_add(1).expect("sequence overflow");
        let key = (at, self.sequence);
        self.queue.insert(key, event);
        key
    }
    fn random(&mut self, domain: &str, context: RequestId) -> u64 {
        let index = self.streams.entry(domain.into()).or_default();
        let mut h = Sha256::new();
        h.update(domain.as_bytes());
        h.update(self.seed.to_le_bytes());
        h.update(index.to_le_bytes());
        h.update(context.to_le_bytes());
        *index += 1;
        u64::from_le_bytes(h.finalize()[..8].try_into().unwrap())
    }
    fn owns(&self, owner: Owner) -> bool {
        self.machines.get(&owner.machine).is_some_and(|m| {
            m.alive && (self.defects.ignore_owner || m.incarnation == owner.incarnation)
        })
    }
    fn reject(&mut self, owner: Owner) {
        self.stale += 1;
        self.completions.entry(owner.request).or_insert(Completion {
            owner: Some(owner),
            kind: CompletionKind::Obsolete,
        });
        self.emit("obsolete_completion", json!(owner));
    }
    fn matches(&self, scope: Scope, ep: Endpoint) -> bool {
        match (scope, ep) {
            (Scope::Machine(a), Endpoint::Machine(b)) => a == b,
            (Scope::Az(a), Endpoint::Machine(b)) => self.machines[&b].az == a,
            (Scope::Az(a), Endpoint::Controller) => a == 0,
            _ => false,
        }
    }
    fn apply(&mut self, event: Event) {
        match event {
            Event::Wake(id) => {
                self.completions.insert(
                    id,
                    Completion {
                        owner: None,
                        kind: CompletionKind::Slept,
                    },
                );
                self.emit("controller_wake", json!({"id":id}));
            }
            Event::Timeout(id) => {
                if self.pending_queries.contains(&id) {
                    self.completions.entry(id).or_insert(Completion {
                        owner: None,
                        kind: CompletionKind::Timeout,
                    });
                    self.emit("query_timeout", json!({"request":id}));
                }
            }
            Event::Wire(frame) => {
                let rules: Vec<_> = self.faults.iter().map(|(&id, r)| (id, r.clone())).collect();
                for (id, r) in rules {
                    let applies = (self.matches(r.from, frame.from)
                        && self.matches(r.to, frame.to))
                        || (r.bidirectional
                            && self.matches(r.to, frame.from)
                            && self.matches(r.from, frame.to));
                    if applies
                        && self.random(&format!("loss:{id}"), frame.id) % r.denominator
                            < r.numerator
                    {
                        *self.drops.entry(id).or_default() += 1;
                        self.emit("frame_drop", json!({"frame":frame,"handle":id}));
                        return;
                    }
                }
                match frame.to {
                    Endpoint::Controller => {
                        self.controller_inbox.push(frame.clone());
                    }
                    Endpoint::Machine(id) => {
                        let m = self.machines.get_mut(&id).unwrap();
                        if !m.alive {
                            self.emit("frame_to_down_machine", json!(frame));
                            return;
                        }
                        m.inbox.push(frame.clone());
                    }
                }
                self.delivered.insert(frame.id, frame.clone());
                self.emit("frame_delivered", json!(frame));
            }
            Event::Callback { owner, kind } => {
                if !self.owns(owner) {
                    self.reject(owner);
                    return;
                }
                self.completions.entry(owner.request).or_insert(Completion {
                    owner: Some(owner),
                    kind,
                });
                self.emit("device_callback", json!(owner));
            }
            Event::Dma {
                owner,
                offset,
                data,
            } => {
                if !self.owns(owner) {
                    self.reject(owner);
                    return;
                }
                self.machines.get_mut(&owner.machine).unwrap().memory[offset..offset + data.len()]
                    .copy_from_slice(&data);
                self.schedule(
                    0,
                    Event::Callback {
                        owner,
                        kind: CompletionKind::Dma,
                    },
                );
                self.emit(
                    "dma_mutation",
                    json!({"owner":owner,"offset":offset,"data":data}),
                );
            }
            Event::Media {
                owner,
                submission,
                offset,
                data,
            } => {
                let m = self.machines.get_mut(&owner.machine).unwrap();
                // Stable medium work survives guest incarnation changes. Refresh only
                // clean cache bytes; dirty current-generation bytes remain volatile.
                for (i, &byte) in data.iter().enumerate() {
                    let at = offset + i;
                    if m.volatile[at] == m.durable[at] {
                        m.volatile[at] = byte;
                    }
                    m.durable[at] = byte;
                }
                if m.incarnation == owner.incarnation {
                    m.pending_writes.remove(&submission);
                }
                self.schedule(
                    0,
                    Event::Callback {
                        owner,
                        kind: CompletionKind::Write,
                    },
                );
                self.emit(
                    "stable_media_effect",
                    json!({"owner":owner,"offset":offset,"data":data}),
                );
            }
            Event::Block {
                owner,
                submission,
                through,
                op,
            } => {
                if !self.owns(owner) {
                    self.reject(owner);
                    return;
                }
                if op == BlockOp::Flush && !self.defects.early_flush {
                    let wait = self.machines[&owner.machine]
                        .pending_writes
                        .range(..=through)
                        .map(|(_, due)| *due)
                        .max();
                    if let Some(at) = wait {
                        self.schedule(
                            at.saturating_sub(self.now),
                            Event::Block {
                                owner,
                                submission,
                                through,
                                op,
                            },
                        );
                        self.emit(
                            "flush_wait",
                            json!({"owner":owner,"through":through,"until":at}),
                        );
                        return;
                    }
                }
                let m = self.machines.get_mut(&owner.machine).unwrap();
                let kind = match &op {
                    BlockOp::Read { offset, len } => {
                        CompletionKind::Read(m.volatile[*offset..offset + len].to_vec())
                    }
                    BlockOp::Write { offset, data } => {
                        m.volatile[*offset..offset + data.len()].copy_from_slice(data);
                        m.pending_writes.remove(&submission);
                        CompletionKind::Write
                    }
                    BlockOp::WriteThrough { .. } => {
                        unreachable!("write-through uses stable media event")
                    }
                    BlockOp::Flush => {
                        m.durable = m.volatile.clone();
                        CompletionKind::Flush
                    }
                };
                self.schedule(0, Event::Callback { owner, kind });
                self.emit(
                    "block_physical_effect",
                    json!({"owner":owner,"op":op,"submission":submission,"through":through}),
                );
            }
        }
    }
}
impl World {
    pub fn new(
        seed: u64,
        machines: &[(MachineId, u32)],
        block_size: usize,
        memory_size: usize,
        defects: Defects,
    ) -> Self {
        let mut w = World {
            schema_version: PROTOCOL_VERSION,
            clock_policy: ClockPolicy::default(),
            channels: BTreeMap::new(),
            control_ingress: VecDeque::new(),
            pending_commands: BTreeMap::new(),
            boundaries: BTreeMap::new(),
            controller_boundaries: BTreeMap::new(),
            seed,
            now: 0,
            sequence: 0,
            next_id: 0,
            machines: machines
                .iter()
                .map(|&(id, az)| {
                    (
                        id,
                        Machine {
                            az,
                            incarnation: 1,
                            alive: true,
                            volatile: vec![0; block_size],
                            durable: vec![0; block_size],
                            memory: vec![0; memory_size],
                            next_submission: 0,
                            pending_writes: BTreeMap::new(),
                            inbox: vec![],
                        },
                    )
                })
                .collect(),
            faults: BTreeMap::new(),
            queue: BTreeMap::new(),
            completions: BTreeMap::new(),
            pending_queries: BTreeSet::new(),
            streams: BTreeMap::new(),
            delivered: BTreeMap::new(),
            drops: BTreeMap::new(),
            backend_state: BTreeMap::new(),
            controller_inbox: vec![],
            stale: 0,
            defects,
            trace: vec![],
        };
        w.emit("initial",json!({"profile":"writeback-cache; completion-order writes; drop-at-delivery; wire survives reboot"}));
        w
    }
    pub fn now(&self) -> TimeTicks {
        self.now
    }
    pub fn incarnation(&self, id: MachineId) -> u64 {
        self.machines[&id].incarnation
    }
    pub fn alive(&self, id: MachineId) -> bool {
        self.machines[&id].alive
    }

    pub fn trace(&self) -> Vec<Value> {
        self.trace.clone()
    }

    pub fn checkpoint_backend(&mut self, name: &str, state: Value) {
        let s = self;
        s.backend_state.insert(name.into(), state);
        s.emit("backend_checkpoint", json!({"name":name}));
    }
    pub fn note(&mut self, label: &str, detail: Value) {
        self.emit(label, detail)
    }
    pub fn owner(&mut self, id: MachineId) -> Owner {
        let s = self;
        assert!(s.machines[&id].alive);
        Owner {
            machine: id,
            incarnation: s.machines[&id].incarnation,
            request: s.id(),
        }
    }
    pub fn transmit(
        &mut self,
        owner: Owner,
        to: Endpoint,
        bytes: Vec<u8>,
        wire_delay: Option<TimeTicks>,
    ) -> Result<RequestId> {
        let s = self;
        if !s.owns(owner) {
            return Err("obsolete transmit owner".into());
        }
        if bytes.len() < 14 {
            return Err("Ethernet frame shorter than 14 bytes".into());
        }
        let id = s.id();
        let delay = wire_delay.unwrap_or_else(|| 2 + s.random("latency", id) % 3);
        s.schedule(
            1,
            Event::Callback {
                owner,
                kind: CompletionKind::Tx,
            },
        );
        s.schedule(
            delay,
            Event::Wire(ReceivedFrame {
                id,
                from: Endpoint::Machine(owner.machine),
                to,
                bytes,
            }),
        );
        s.emit(
            "frame_transmit",
            json!({"owner":owner,"wire_id":id,"delay":delay}),
        );
        Ok(id)
    }
    /// Controller is an immune continuation, with a faultable AZ0 fabric endpoint.
    pub fn controller_frame(
        &mut self,
        to: MachineId,
        bytes: Vec<u8>,
        delay: Option<TimeTicks>,
    ) -> RequestId {
        let s = self;
        assert!(bytes.len() >= 14);
        let id = s.id();
        let d = delay.unwrap_or_else(|| 2 + s.random("latency", id) % 3);
        s.schedule(
            d,
            Event::Wire(ReceivedFrame {
                id,
                from: Endpoint::Controller,
                to: Endpoint::Machine(to),
                bytes,
            }),
        );
        s.emit("controller_frame", json!({"id":id,"machine":to}));
        id
    }
    pub fn take_frames(&mut self, ep: Endpoint) -> Vec<ReceivedFrame> {
        let s = self;
        let frames = match ep {
            Endpoint::Controller => std::mem::take(&mut s.controller_inbox),
            Endpoint::Machine(id) => std::mem::take(&mut s.machines.get_mut(&id).unwrap().inbox),
        };
        if !frames.is_empty() {
            s.emit(
                "backend_take_frames",
                json!({"endpoint":ep,"frames":frames}),
            );
        }
        frames
    }
    pub fn block(&mut self, owner: Owner, op: BlockOp, delay: TimeTicks) -> Result<()> {
        let s = self;
        if !s.owns(owner) {
            return Err("obsolete block owner".into());
        }
        let due = s.now.checked_add(delay).ok_or("time overflow")?;
        let m = s.machines.get_mut(&owner.machine).unwrap();
        let valid = match &op {
            BlockOp::Read { offset, len } => offset
                .checked_add(*len)
                .is_some_and(|end| end <= m.volatile.len()),
            BlockOp::Write { offset, data } | BlockOp::WriteThrough { offset, data } => offset
                .checked_add(data.len())
                .is_some_and(|end| end <= m.volatile.len()),
            BlockOp::Flush => true,
        };
        if !valid {
            return Err("block range outside medium".into());
        }
        let through = m.next_submission;
        m.next_submission += 1;
        let submission = m.next_submission;
        if matches!(op, BlockOp::Write { .. } | BlockOp::WriteThrough { .. }) {
            m.pending_writes.insert(submission, due);
        }
        let event = match &op {
            BlockOp::WriteThrough { offset, data } => Event::Media {
                owner,
                submission,
                offset: *offset,
                data: data.clone(),
            },
            _ => Event::Block {
                owner,
                submission,
                through,
                op: op.clone(),
            },
        };
        s.schedule(delay, event);
        s.emit(
            "block_submit",
            json!({"owner":owner,"op":op,"through":through,"submission":submission,"due":due}),
        );
        Ok(())
    }
    pub fn dma(
        &mut self,
        owner: Owner,
        offset: usize,
        data: Vec<u8>,
        delay: TimeTicks,
    ) -> Result<()> {
        let s = self;
        if !s.owns(owner) {
            return Err("obsolete DMA owner".into());
        }
        if offset
            .checked_add(data.len())
            .is_none_or(|end| end > s.machines[&owner.machine].memory.len())
        {
            return Err("DMA outside RAM".into());
        }
        s.schedule(
            delay,
            Event::Dma {
                owner,
                offset,
                data,
            },
        );
        s.emit("dma_submit", json!(owner));
        Ok(())
    }
    pub fn completion(&mut self, id: RequestId) -> Option<Completion> {
        let s = self;
        let c = s.take_completion(id);
        if c.is_some() {
            s.emit("completion_consumed", json!({"id":id,"completion":c}));
        }
        c
    }

    /// Backend protocol adapter resolves its decoded reply. First terminal event wins.
    pub fn resolve_query(&mut self, id: RequestId, data: Vec<u8>) {
        let s = self;
        if s.pending_queries.contains(&id) {
            s.completions.entry(id).or_insert(Completion {
                owner: None,
                kind: CompletionKind::Reply(data),
            });
            s.emit("query_reply", json!({"id":id}));
        }
    }
    pub fn fault(&mut self, rule: LossRule) -> Result<FaultHandle> {
        if rule.denominator == 0 || rule.numerator > rule.denominator {
            return Err("invalid loss fraction".into());
        }
        let s = self;
        let id = s.id();
        s.faults.insert(id.0, rule.clone());
        s.emit("fault_apply", json!({"handle":id,"rule":rule}));
        Ok(FaultHandle(id.0))
    }
    pub fn heal(&mut self, handle: FaultHandle) -> Result<()> {
        let s = self;
        if s.faults.remove(&handle.0).is_none() {
            return Err("unknown fault handle".into());
        }
        if s.defects.erase_overlap {
            s.faults.clear();
        }
        s.emit("fault_heal", json!({"handle":handle}));
        Ok(())
    }
    pub fn crash(&mut self, id: MachineId) {
        let s = self;
        let m = s.machines.get_mut(&id).unwrap();
        assert!(m.alive);
        m.alive = false;
        m.volatile = m.durable.clone();
        m.pending_writes.clear();
        m.memory.fill(0);
        m.inbox.clear();
        if s.defects.cancel_wire {
            s.queue.retain(|_,ev|!matches!(ev,Event::Wire(f) if f.from==Endpoint::Machine(id)||f.to==Endpoint::Machine(id)));
        }
        if s.defects.cancel_media {
            s.queue
                .retain(|_, ev| !matches!(ev,Event::Media{owner,..} if owner.machine==id));
        }
        s.boundaries.remove(&id);
        s.emit("machine_crash", json!({"machine":id}));
    }
    pub fn reboot(&mut self, id: MachineId) {
        let s = self;
        let m = s.machines.get_mut(&id).unwrap();
        assert!(!m.alive);
        m.incarnation += 1;
        m.alive = true;
        m.next_submission = 0;
        s.emit("machine_reboot", json!({"machine":id}));
    }
    pub fn bytes(&self, id: MachineId, durable: bool) -> Vec<u8> {
        let s = self;
        let m = &s.machines[&id];
        if durable {
            m.durable.clone()
        } else {
            m.volatile.clone()
        }
    }
    pub fn memory(&self, id: MachineId) -> Vec<u8> {
        self.machines[&id].memory.clone()
    }
    pub fn delivered(&self, id: RequestId) -> Option<ReceivedFrame> {
        self.delivered.get(&id).cloned()
    }
    pub fn stale_count(&self) -> u64 {
        self.stale
    }

    /// One registered callback. Caller must finish a cooperative backend boundary
    /// before invoking this; no runnable CPU time or production cost is invented.
    fn step(&mut self) -> bool {
        let s = self;
        let Some((key, event)) = s.queue.pop_first() else {
            return false;
        };
        s.now = s.now.max(key.0);
        s.emit(
            "event_dispatch",
            json!({"registration":key.1,"event":event}),
        );
        s.apply(event);
        if s.defects.batch_events {
            while s.queue.first_key_value().is_some_and(|(k, _)| k.0 == s.now) {
                let (k, event) = s.queue.pop_first().unwrap();
                s.emit("event_dispatch", json!({"registration":k.1,"event":event}));
                s.apply(event);
            }
        }
        true
    }
    pub fn check(&mut self, condition: bool, label: &str) -> Result<()> {
        self.note(
            "application_check",
            json!({"label":label,"passed":condition}),
        );
        if condition { Ok(()) } else { Err(label.into()) }
    }
}
mod event_queue {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        q: &BTreeMap<(TimeTicks, u64), Event>,
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        q.iter()
            .map(|(&(tick, registration), event)| (tick, registration, event))
            .collect::<Vec<_>>()
            .serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<BTreeMap<(TimeTicks, u64), Event>, D::Error> {
        let rows = Vec::<(TimeTicks, u64, Event)>::deserialize(d)?;
        let mut q = BTreeMap::new();
        for (tick, registration, event) in rows {
            if q.insert((tick, registration), event).is_some() {
                return Err(serde::de::Error::custom("duplicate event key"));
            }
        }
        Ok(q)
    }
}
