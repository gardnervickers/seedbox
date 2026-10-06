//! Versioned reliable control-device protocol, independent of Ethernet fault policy.
use super::*;
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_CONTROL_FRAME: usize = 16 * 1024 * 1024;
macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}
id_type!(ControllerId);
id_type!(CommandId);
id_type!(ObservationSequence);
id_type!(BranchCount);
id_type!(Tick);
id_type!(LinuxNanoseconds);
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ClockPolicy {
    pub linux_ns_numerator: u64,
    pub branch_tick_denominator: u64,
}
impl Default for ClockPolicy {
    fn default() -> Self {
        Self {
            linux_ns_numerator: 1,
            branch_tick_denominator: 1,
        }
    }
}
impl ClockPolicy {
    pub fn linux_ns(&self, tick: Tick) -> Result<LinuxNanoseconds> {
        if self.branch_tick_denominator == 0 {
            return Err("zero tick conversion denominator".into());
        }
        let ns = u128::from(tick.0) * u128::from(self.linux_ns_numerator)
            / u128::from(self.branch_tick_denominator);
        Ok(LinuxNanoseconds(
            u64::try_from(ns).map_err(|_| "Linux ns conversion overflow")?,
        ))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryKind {
    CooperativeHook,
    DeviceExit,
    PrecisePreemption,
    Blocked,
    Halted,
    Shutdown,
    Fault,
    CompletionOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CounterOrigin {
    HardwareReported,
    ScriptedModel,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepProgress {
    pub rcx: u64,
    pub rsi: u64,
    pub rdi: u64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum GuestOwner {
    Machine(Owner),
    ScenarioVm {
        controller: ControllerId,
        incarnation: u64,
    },
}
impl GuestOwner {
    fn incarnation(self) -> u64 {
        match self {
            Self::Machine(owner) => owner.incarnation,
            Self::ScenarioVm { incarnation, .. } => incarnation,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoppedGuestBoundary {
    pub owner: GuestOwner,
    pub boundary_sequence: u64,
    pub rip: u64,
    pub rep: Option<RepProgress>,
    pub kind: BoundaryKind,
    pub counter_origin: CounterOrigin,
    pub raw_counter: Option<BranchCount>,
    pub accumulated_guest_branches: BranchCount,
    pub delta_guest_branches: BranchCount,
    pub requested_target: Option<BranchCount>,
    pub actual_target: Option<BranchCount>,
    pub overshoot: Option<BranchCount>,
    pub pending_io_completed: bool,
    pub tick_before: Tick,
    pub tick_after: Tick,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Inspect {
    Frame { wire_id: RequestId },
    Storage { machine: MachineId, durable: bool },
    Memory { machine: MachineId },
    StaleCount,
    Machine { machine: MachineId },
    Time,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Sleep {
        after_ticks: Tick,
    },
    ArmSleep {
        after_ticks: Tick,
    },
    Fault {
        rule: LossRule,
    },
    Heal {
        handle: FaultHandle,
    },
    Crash {
        machine: MachineId,
    },
    Reboot {
        machine: MachineId,
    },
    Transmit {
        from: MachineId,
        to: Endpoint,
        bytes: Vec<u8>,
        wire_delay_ticks: Option<Tick>,
    },
    Block {
        machine: MachineId,
        op: BlockOp,
        delay_ticks: Tick,
    },
    Dma {
        machine: MachineId,
        offset: usize,
        data: Vec<u8>,
        delay_ticks: Tick,
    },
    Await {
        request: RequestId,
    },
    Query {
        destination: MachineId,
        frame: Vec<u8>,
        correlation_u64_offset: usize,
        timeout_ticks: Tick,
    },
    Inspect(Inspect),
    Assert {
        label: String,
        condition: bool,
    },
    Note {
        label: String,
        detail: Value,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub version: u32,
    pub controller: ControllerId,
    pub incarnation: u64,
    pub id: CommandId,
    pub command: Command,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Observation {
    SleepArmed {
        request: RequestId,
    },
    Slept {
        at_tick: Tick,
    },
    FaultApplied(FaultHandle),
    Healed,
    Lifecycle {
        machine: MachineId,
        incarnation: u64,
        alive: bool,
    },
    Submitted(Owner),
    Transmitted {
        owner: Owner,
        wire_id: RequestId,
    },
    Completion(Completion),
    Inspected(Value),
    Assertion {
        label: String,
        passed: bool,
    },
    Noted,
    Error(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservationEnvelope {
    pub version: u32,
    pub controller: ControllerId,
    pub incarnation: u64,
    pub sequence: ObservationSequence,
    pub command: CommandId,
    pub observation: Observation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ControlMessage {
    Command(CommandEnvelope),
    Observation(ObservationEnvelope),
    Ack {
        version: u32,
        controller: ControllerId,
        incarnation: u64,
        sequence: ObservationSequence,
    },
}
/// Little-endian u32 length followed by UTF-8 JSON. Version/ordered IDs are inside.
pub fn encode_control(message: &ControlMessage) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(message).map_err(|e| e.to_string())?;
    if json.len() > MAX_CONTROL_FRAME {
        return Err("control frame too large".into());
    }
    let mut bytes = (json.len() as u32).to_le_bytes().to_vec();
    bytes.extend(json);
    Ok(bytes)
}
pub fn decode_control(bytes: &[u8]) -> Result<ControlMessage> {
    if bytes.len() < 4 {
        return Err("truncated control length".into());
    }
    let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    if len > MAX_CONTROL_FRAME || bytes.len() != len + 4 {
        return Err("invalid control length".into());
    }
    let msg: ControlMessage = serde_json::from_slice(&bytes[4..]).map_err(|e| e.to_string())?;
    let version = match &msg {
        ControlMessage::Command(e) => e.version,
        ControlMessage::Observation(e) => e.version,
        ControlMessage::Ack { version, .. } => *version,
    };
    if version != PROTOCOL_VERSION {
        return Err("unsupported control version".into());
    }
    Ok(msg)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ReliableChannel {
    incarnation: u64,
    next_command: u64,
    next_observation: u64,
    last_acked: u64,
    accepted: BTreeMap<CommandId, CommandEnvelope>,
    observations: VecDeque<ObservationEnvelope>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum PendingStatus {
    Queued,
    Waiting { world_request: RequestId },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PendingCommand {
    envelope: CommandEnvelope,
    status: PendingStatus,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DispatchOutcome {
    Command {
        controller: ControllerId,
        id: CommandId,
    },
    Event {
        tick: Tick,
        registration: u64,
    },
    ControlReady {
        controller: ControllerId,
    },
    GuestProgressRequired,
    Quiescent,
}
impl World {
    pub fn register_controller(&mut self, id: ControllerId, incarnation: u64) -> Result<()> {
        if self.channels.contains_key(&id) {
            return Err("controller already registered".into());
        }
        self.channels.insert(
            id,
            ReliableChannel {
                incarnation,
                next_command: 1,
                next_observation: 1,
                last_acked: 0,
                accepted: BTreeMap::new(),
                observations: VecDeque::new(),
            },
        );
        self.note("control_registered",json!({"controller":id,"incarnation":incarnation,"transport":"reliable_control_device; outside faultable fabric"}));
        Ok(())
    }
    pub fn receive_control(&mut self, bytes: &[u8]) -> Result<()> {
        match decode_control(bytes)? {
            ControlMessage::Command(envelope) => {
                let channel = self
                    .channels
                    .get_mut(&envelope.controller)
                    .ok_or("unknown controller")?;
                if channel.incarnation != envelope.incarnation {
                    return Err("obsolete controller incarnation".into());
                }
                if let Some(previous) = channel.accepted.get(&envelope.id) {
                    if serde_json::to_value(previous).unwrap()
                        != serde_json::to_value(&envelope).unwrap()
                    {
                        return Err("command ID reused with different payload".into());
                    }
                    return Ok(());
                }
                if envelope.id.0 != channel.next_command {
                    return Err("lost or reordered control command".into());
                }
                channel.next_command = channel
                    .next_command
                    .checked_add(1)
                    .ok_or("command ID overflow")?;
                channel.accepted.insert(envelope.id, envelope.clone());
                let pending = self.id();
                self.pending_commands.insert(
                    pending,
                    PendingCommand {
                        envelope: envelope.clone(),
                        status: PendingStatus::Queued,
                    },
                );
                self.control_ingress.push_back(pending);
                self.note(
                    "control_command_received",
                    json!({"pending":pending,"envelope":envelope}),
                );
                Ok(())
            }
            ControlMessage::Ack {
                controller,
                incarnation,
                sequence,
                ..
            } => {
                let channel = self
                    .channels
                    .get_mut(&controller)
                    .ok_or("unknown controller")?;
                if channel.incarnation != incarnation {
                    return Err("obsolete observation ack".into());
                }
                if sequence.0 <= channel.last_acked {
                    return Ok(());
                }
                let front = channel
                    .observations
                    .front()
                    .ok_or("ack without observation")?;
                if front.sequence != sequence {
                    return Err("lost or reordered observation ack".into());
                }
                channel.observations.pop_front();
                channel.last_acked = sequence.0;
                self.note(
                    "control_observation_acked",
                    json!({"controller":controller,"sequence":sequence}),
                );
                Ok(())
            }
            ControlMessage::Observation(_) => Err("observation on command ingress".into()),
        }
    }
    pub fn peek_control(&self, controller: ControllerId) -> Result<Option<Vec<u8>>> {
        let channel = self.channels.get(&controller).ok_or("unknown controller")?;
        channel
            .observations
            .front()
            .map(|e| encode_control(&ControlMessage::Observation(e.clone())))
            .transpose()
    }
    fn finish_command(&mut self, pending: RequestId, observation: Observation) {
        let command = self
            .pending_commands
            .remove(&pending)
            .expect("pending command exists");
        let e = command.envelope;
        let channel = self.channels.get_mut(&e.controller).unwrap();
        let sequence = ObservationSequence(channel.next_observation);
        channel.next_observation += 1;
        let observation = ObservationEnvelope {
            version: PROTOCOL_VERSION,
            controller: e.controller,
            incarnation: e.incarnation,
            sequence,
            command: e.id,
            observation,
        };
        channel.observations.push_back(observation.clone());
        self.note(
            "control_observation_queued",
            json!({"pending":pending,"observation":observation}),
        );
    }
    fn run_command(&mut self, pending: RequestId) -> Result<Option<Observation>> {
        let cmd = self.pending_commands[&pending].envelope.command.clone();
        Ok(match cmd {
            Command::Sleep { after_ticks } => {
                let request = self.id();
                self.schedule(after_ticks.0, Event::Wake(request));
                self.pending_commands.get_mut(&pending).unwrap().status = PendingStatus::Waiting {
                    world_request: request,
                };
                None
            }
            Command::ArmSleep { after_ticks } => {
                let request = self.id();
                self.schedule(after_ticks.0, Event::Wake(request));
                Some(Observation::SleepArmed { request })
            }
            Command::Fault { rule } => Some(Observation::FaultApplied(self.fault(rule)?)),
            Command::Heal { handle } => {
                self.heal(handle)?;
                Some(Observation::Healed)
            }
            Command::Crash { machine } => {
                if !self.machines.contains_key(&machine) || !self.alive(machine) {
                    return Err("crash requires live workload machine".into());
                }
                self.crash(machine);
                Some(Observation::Lifecycle {
                    machine,
                    incarnation: self.incarnation(machine),
                    alive: false,
                })
            }
            Command::Reboot { machine } => {
                if !self.machines.contains_key(&machine) || self.alive(machine) {
                    return Err("reboot requires stopped workload machine".into());
                }
                self.reboot(machine);
                Some(Observation::Lifecycle {
                    machine,
                    incarnation: self.incarnation(machine),
                    alive: true,
                })
            }
            Command::Transmit {
                from,
                to,
                bytes,
                wire_delay_ticks,
            } => {
                if !self.machines.contains_key(&from) || !self.alive(from) {
                    return Err("transmit requires live workload machine".into());
                }
                let owner = self.owner(from);
                let wire_id = self.transmit(owner, to, bytes, wire_delay_ticks.map(|t| t.0))?;
                Some(Observation::Transmitted { owner, wire_id })
            }
            Command::Block {
                machine,
                op,
                delay_ticks,
            } => {
                if !self.machines.contains_key(&machine) || !self.alive(machine) {
                    return Err("block requires live workload machine".into());
                }
                let owner = self.owner(machine);
                self.block(owner, op, delay_ticks.0)?;
                Some(Observation::Submitted(owner))
            }
            Command::Dma {
                machine,
                offset,
                data,
                delay_ticks,
            } => {
                if !self.machines.contains_key(&machine) || !self.alive(machine) {
                    return Err("DMA requires live workload machine".into());
                }
                let owner = self.owner(machine);
                self.dma(owner, offset, data, delay_ticks.0)?;
                Some(Observation::Submitted(owner))
            }
            Command::Await { request } => {
                if self.pending_commands.iter().any(|(&id,p)|id!=pending&&matches!(p.status,PendingStatus::Waiting{world_request} if world_request==request)){return Err("duplicate request waiter".into());}
                if let Some(c) = self.completion(request) {
                    Some(Observation::Completion(c))
                } else {
                    self.pending_commands.get_mut(&pending).unwrap().status =
                        PendingStatus::Waiting {
                            world_request: request,
                        };
                    None
                }
            }
            Command::Query {
                destination,
                mut frame,
                correlation_u64_offset,
                timeout_ticks,
            } => {
                if !self.machines.contains_key(&destination) {
                    return Err("query destination missing".into());
                }
                let end = correlation_u64_offset
                    .checked_add(8)
                    .ok_or("query correlation overflow")?;
                if end > frame.len() || frame.len() < 14 {
                    return Err("query correlation outside frame".into());
                }
                let request = self.id();
                frame[correlation_u64_offset..end].copy_from_slice(&request.to_le_bytes());
                self.pending_queries.insert(request);
                self.schedule(timeout_ticks.0, Event::Timeout(request));
                self.controller_frame(destination, frame, None);
                self.pending_commands.get_mut(&pending).unwrap().status = PendingStatus::Waiting {
                    world_request: request,
                };
                None
            }
            Command::Inspect(inspect) => {
                if let Inspect::Storage { machine, .. }
                | Inspect::Memory { machine }
                | Inspect::Machine { machine } = &inspect
                    && !self.machines.contains_key(machine)
                {
                    return Err("inspection machine missing".into());
                }
                let data = match inspect {
                    Inspect::Frame { wire_id } => json!(self.delivered(wire_id)),
                    Inspect::Storage { machine, durable } => json!(self.bytes(machine, durable)),
                    Inspect::Memory { machine } => json!(self.memory(machine)),
                    Inspect::StaleCount => json!(self.stale_count()),
                    Inspect::Machine { machine } => {
                        json!({"machine":machine,"incarnation":self.incarnation(machine),"alive":self.alive(machine)})
                    }
                    Inspect::Time => {
                        json!({"tick":self.now,"linux_ns":self.clock_policy.linux_ns(Tick(self.now))?})
                    }
                };
                Some(Observation::Inspected(data))
            }
            Command::Assert { label, condition } => {
                self.note(
                    "application_check",
                    json!({"label":label,"passed":condition}),
                );
                Some(Observation::Assertion {
                    label,
                    passed: condition,
                })
            }
            Command::Note { label, detail } => {
                self.note(&label, detail);
                Some(Observation::Noted)
            }
        })
    }
    /// Execute one ingress command or one world event. An unacknowledged reliable
    /// observation blocks the next world event, giving the controller its turn.
    pub fn dispatch_one(&mut self) -> DispatchOutcome {
        if let Some(pending) = self.control_ingress.pop_front() {
            let envelope = self.pending_commands[&pending].envelope.clone();
            match self.run_command(pending) {
                Ok(Some(o)) => self.finish_command(pending, o),
                Ok(None) => {}
                Err(e) => self.finish_command(pending, Observation::Error(e)),
            };
            return DispatchOutcome::Command {
                controller: envelope.controller,
                id: envelope.id,
            };
        }
        if let Some((&controller, _)) = self
            .channels
            .iter()
            .find(|(_, c)| !c.observations.is_empty())
        {
            return DispatchOutcome::ControlReady { controller };
        }
        self.complete_backend_boundary();
        if let Some((&controller, _)) = self
            .channels
            .iter()
            .find(|(_, c)| !c.observations.is_empty())
        {
            return DispatchOutcome::ControlReady { controller };
        }
        if let Some((&(tick, registration), _)) = self.queue.first_key_value() {
            if tick > self.now
                && self
                    .boundaries
                    .values()
                    .chain(self.controller_boundaries.values())
                    .any(|b| {
                        !matches!(
                            b.kind,
                            BoundaryKind::Blocked
                                | BoundaryKind::Halted
                                | BoundaryKind::Shutdown
                                | BoundaryKind::Fault
                        )
                    })
            {
                return DispatchOutcome::GuestProgressRequired;
            }
            self.step();
            return DispatchOutcome::Event {
                tick: Tick(self.now),
                registration,
            };
        }
        DispatchOutcome::Quiescent
    }
    /// Call after the backend finished all pending guest I/O and model callbacks.
    pub fn complete_backend_boundary(&mut self) {
        let waits: Vec<_> = self
            .pending_commands
            .iter()
            .filter_map(|(&id, p)| match p.status {
                PendingStatus::Waiting { world_request }
                    if self.completions.contains_key(&world_request) =>
                {
                    Some((id, world_request))
                }
                _ => None,
            })
            .collect();
        for (pending, request) in waits {
            let c = self.completion(request).unwrap();
            self.pending_queries.remove(&request);
            let o = if c.kind == CompletionKind::Slept {
                Observation::Slept {
                    at_tick: Tick(self.now),
                }
            } else {
                Observation::Completion(c)
            };
            self.finish_command(pending, o);
        }
        self.note("completed_boundary", json!({"tick":self.now}));
    }
    pub fn set_clock_policy(&mut self, policy: ClockPolicy) -> Result<()> {
        policy.linux_ns(Tick(self.now))?;
        self.clock_policy = policy;
        Ok(())
    }
    /// Imported stopped-guest declaration, not evidence that the host can preempt.
    /// Debugger administration is outside this semantic sequence. Natural PIO/MMIO
    /// exits during correction remain DeviceExit boundaries; charge guest branches once.
    /// Branch delta is the only executed-guest time input. Completion-only work
    /// consumes none. Scripted fixtures may not claim PrecisePreemption.
    pub fn import_stopped_guest(&mut self, b: StoppedGuestBoundary) -> Result<()> {
        let valid_owner = match b.owner {
            GuestOwner::Machine(owner) => self.owns(owner),
            GuestOwner::ScenarioVm {
                controller,
                incarnation,
            } => self
                .channels
                .get(&controller)
                .is_some_and(|c| c.incarnation == incarnation),
        };
        if !valid_owner || !b.pending_io_completed {
            return Err("guest boundary ownership/I/O incomplete".into());
        }
        if b.tick_before.0 != self.now
            || b.tick_after.0
                != self
                    .now
                    .checked_add(b.delta_guest_branches.0)
                    .ok_or("tick overflow")?
        {
            return Err("guest branch delta/tick mismatch".into());
        }
        if b.kind == BoundaryKind::CompletionOnly && b.delta_guest_branches.0 != 0 {
            return Err("completion-only boundary charged progress".into());
        }
        if b.kind == BoundaryKind::CompletionOnly {
            // Completion/debug administration is outside canonical numbering,
            // timing and world event dispatch; commit its enclosing semantic exit once.
            return Ok(());
        }
        if b.kind == BoundaryKind::PrecisePreemption
            && (b.counter_origin != CounterOrigin::HardwareReported
                || b.requested_target != b.actual_target
                || b.requested_target.is_none()
                || b.overshoot != Some(BranchCount(0)))
        {
            return Err("precise-preemption contract unsatisfied".into());
        }
        let previous = match b.owner {
            GuestOwner::Machine(owner) => self.boundaries.get(&owner.machine),
            GuestOwner::ScenarioVm { controller, .. } => {
                self.controller_boundaries.get(&controller)
            }
        };
        if let Some(previous) = previous
            && previous.owner.incarnation() == b.owner.incarnation()
            && (b.boundary_sequence <= previous.boundary_sequence
                || b.accumulated_guest_branches.0
                    != previous.accumulated_guest_branches.0 + b.delta_guest_branches.0)
        {
            return Err("guest boundary sequence/counter mismatch".into());
        }
        self.clock_policy.linux_ns(b.tick_after)?;
        self.now = b.tick_after.0;
        match b.owner {
            GuestOwner::Machine(owner) => {
                self.boundaries.insert(owner.machine, b.clone());
            }
            GuestOwner::ScenarioVm { controller, .. } => {
                self.controller_boundaries.insert(controller, b.clone());
            }
        }
        self.note("stopped_guest_boundary", json!(b));
        Ok(())
    }
    pub fn encode_state(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| e.to_string())
    }
    pub fn decode_state(bytes: &[u8]) -> Result<Self> {
        let world: World = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        world.validate_state()?;
        Ok(world)
    }
    pub fn validate_state(&self) -> Result<()> {
        if self.schema_version != PROTOCOL_VERSION {
            return Err("unsupported world state version".into());
        }
        self.clock_policy.linux_ns(Tick(self.now))?;
        let mut ingress_order: BTreeMap<ControllerId, u64> = BTreeMap::new();
        for id in &self.control_ingress {
            let p = self
                .pending_commands
                .get(id)
                .ok_or("ingress pending command missing")?;
            let last = ingress_order.entry(p.envelope.controller).or_default();
            if p.envelope.id.0 <= *last {
                return Err("pending control commands reordered".into());
            }
            *last = p.envelope.id.0;
        }
        let queued: BTreeSet<_> = self.control_ingress.iter().copied().collect();
        if queued.len() != self.control_ingress.len() {
            return Err("duplicate pending control command".into());
        }
        for (&id, p) in &self.pending_commands {
            match p.status {
                PendingStatus::Queued if !queued.contains(&id) => {
                    return Err("pending control command lost".into());
                }
                PendingStatus::Waiting { .. } if queued.contains(&id) => {
                    return Err("waiting command queued twice".into());
                }
                _ => {}
            }
            let channel = self
                .channels
                .get(&p.envelope.controller)
                .ok_or("pending command channel missing")?;
            if channel.incarnation != p.envelope.incarnation
                || !channel.accepted.contains_key(&p.envelope.id)
            {
                return Err("pending command ownership missing".into());
            }
        }
        for id in queued {
            if !self.pending_commands.contains_key(&id) {
                return Err("ingress pending command missing".into());
            }
        }
        for channel in self.channels.values() {
            let mut seq = channel.last_acked + 1;
            for e in &channel.observations {
                if e.sequence.0 != seq {
                    return Err("pending observations reordered".into());
                }
                seq += 1;
            }
            if seq != channel.next_observation {
                return Err("pending observation lost".into());
            }
        }
        Ok(())
    }
    /// Deliberately broken restore input for a sensitive research negative control.
    pub fn lose_pending_command_control(&mut self) -> bool {
        self.control_ingress.pop_front().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn world() -> World {
        let mut w = World::new(
            42,
            &[(MachineId(1), 1), (MachineId(2), 2)],
            8,
            8,
            Defects::default(),
        );
        w.register_controller(ControllerId(1), 1).unwrap();
        w
    }
    fn envelope(id: u64, command: Command) -> ControlMessage {
        ControlMessage::Command(CommandEnvelope {
            version: PROTOCOL_VERSION,
            controller: ControllerId(1),
            incarnation: 1,
            id: CommandId(id),
            command,
        })
    }
    fn send(w: &mut World, id: u64, cmd: Command) {
        w.receive_control(&encode_control(&envelope(id, cmd)).unwrap())
            .unwrap();
        assert!(matches!(w.dispatch_one(), DispatchOutcome::Command { .. }));
    }
    fn take(w: &mut World) -> Observation {
        let wire = w.peek_control(ControllerId(1)).unwrap().unwrap();
        let ControlMessage::Observation(e) = decode_control(&wire).unwrap() else {
            panic!()
        };
        w.receive_control(
            &encode_control(&ControlMessage::Ack {
                version: PROTOCOL_VERSION,
                controller: e.controller,
                incarnation: e.incarnation,
                sequence: e.sequence,
            })
            .unwrap(),
        )
        .unwrap();
        e.observation
    }
    #[test]
    fn length_version_and_roundtrip_are_checked() {
        let msg = envelope(
            1,
            Command::Sleep {
                after_ticks: Tick(5),
            },
        );
        let wire = encode_control(&msg).unwrap();
        assert_eq!(
            serde_json::to_value(decode_control(&wire).unwrap()).unwrap(),
            serde_json::to_value(msg).unwrap()
        );
        assert!(decode_control(&wire[..wire.len() - 1]).is_err());
        assert!(decode_control(&[0]).is_err());
    }
    #[test]
    fn control_observation_is_retained_until_ack_and_survives_restore() {
        let mut w = world();
        send(
            &mut w,
            1,
            Command::Fault {
                rule: LossRule {
                    from: Scope::Az(0),
                    to: Scope::Az(1),
                    bidirectional: false,
                    numerator: 1,
                    denominator: 1,
                },
            },
        );
        let a = w.peek_control(ControllerId(1)).unwrap();
        assert_eq!(a, w.peek_control(ControllerId(1)).unwrap());
        let mut restored = World::decode_state(&w.encode_state().unwrap()).unwrap();
        assert_eq!(a, restored.peek_control(ControllerId(1)).unwrap());
        assert!(matches!(
            restored.dispatch_one(),
            DispatchOutcome::ControlReady { .. }
        ));
        take(&mut restored);
        assert!(restored.peek_control(ControllerId(1)).unwrap().is_none());
    }
    #[test]
    fn immune_control_continues_when_every_workload_machine_is_down() {
        let mut w = world();
        send(
            &mut w,
            1,
            Command::Crash {
                machine: MachineId(1),
            },
        );
        take(&mut w);
        send(
            &mut w,
            2,
            Command::Crash {
                machine: MachineId(2),
            },
        );
        take(&mut w);
        send(
            &mut w,
            3,
            Command::Sleep {
                after_ticks: Tick(5),
            },
        );
        assert!(matches!(w.dispatch_one(), DispatchOutcome::Event { .. }));
        w.complete_backend_boundary();
        assert!(matches!(
            take(&mut w),
            Observation::Slept { at_tick: Tick(5) }
        ));
        send(
            &mut w,
            4,
            Command::Assert {
                label: "scenario assertion can fail".into(),
                condition: false,
            },
        );
        assert!(matches!(
            take(&mut w),
            Observation::Assertion { passed: false, .. }
        ));
    }
    #[test]
    fn duplicate_commands_are_idempotent_and_reordering_is_rejected() {
        let mut w = world();
        let bytes = encode_control(&envelope(
            1,
            Command::Sleep {
                after_ticks: Tick(5),
            },
        ))
        .unwrap();
        w.receive_control(&bytes).unwrap();
        let state = w.snapshot();
        w.receive_control(&bytes).unwrap();
        assert_eq!(state, w.snapshot());
        assert_eq!(
            w.receive_control(
                &encode_control(&envelope(
                    3,
                    Command::Sleep {
                        after_ticks: Tick(1)
                    }
                ))
                .unwrap()
            )
            .unwrap_err(),
            "lost or reordered control command"
        );
        assert_eq!(
            w.receive_control(
                &encode_control(&envelope(
                    1,
                    Command::Sleep {
                        after_ticks: Tick(9)
                    }
                ))
                .unwrap()
            )
            .unwrap_err(),
            "command ID reused with different payload"
        );
    }
    #[test]
    fn lost_and_reordered_pending_commands_fail_restore_validation() {
        let mut w = world();
        for id in 1..=2 {
            w.receive_control(
                &encode_control(&envelope(
                    id,
                    Command::Sleep {
                        after_ticks: Tick(id),
                    },
                ))
                .unwrap(),
            )
            .unwrap();
        }
        let mut lost = w.clone();
        assert!(lost.lose_pending_command_control());
        assert_eq!(
            World::decode_state(&lost.encode_state().unwrap()).unwrap_err(),
            "pending control command lost"
        );
        w.control_ingress.swap(0, 1);
        assert_eq!(
            World::decode_state(&w.encode_state().unwrap()).unwrap_err(),
            "pending control commands reordered"
        );
    }
    fn boundary(owner: Owner, delta: u64) -> StoppedGuestBoundary {
        StoppedGuestBoundary {
            owner: GuestOwner::Machine(owner),
            boundary_sequence: 1,
            rip: 0x1000,
            rep: None,
            kind: BoundaryKind::CooperativeHook,
            counter_origin: CounterOrigin::ScriptedModel,
            raw_counter: None,
            accumulated_guest_branches: BranchCount(delta),
            delta_guest_branches: BranchCount(delta),
            requested_target: None,
            actual_target: None,
            overshoot: None,
            pending_io_completed: true,
            tick_before: Tick(0),
            tick_after: Tick(delta),
        }
    }
    #[test]
    fn branch_ticks_and_linux_ns_rate_are_declared_integer_inputs() {
        let mut w = world();
        w.set_clock_policy(ClockPolicy {
            linux_ns_numerator: 3,
            branch_tick_denominator: 2,
        })
        .unwrap();
        let owner = w.owner(MachineId(1));
        w.import_stopped_guest(boundary(owner, 7)).unwrap();
        assert_eq!(w.now(), 7);
        assert_eq!(
            w.clock_policy.linux_ns(Tick(w.now())).unwrap(),
            LinuxNanoseconds(10)
        );
        send(
            &mut w,
            1,
            Command::Sleep {
                after_ticks: Tick(10),
            },
        );
        assert!(matches!(
            w.dispatch_one(),
            DispatchOutcome::GuestProgressRequired
        ));
    }
    #[test]
    fn administration_cannot_charge_ticks_and_scripted_preemption_is_rejected() {
        let mut w = world();
        let owner = w.owner(MachineId(1));
        let mut b = boundary(owner, 1);
        b.kind = BoundaryKind::CompletionOnly;
        assert_eq!(
            w.import_stopped_guest(b).unwrap_err(),
            "completion-only boundary charged progress"
        );
        let before = w.snapshot();
        let mut administration = boundary(owner, 0);
        administration.kind = BoundaryKind::CompletionOnly;
        w.import_stopped_guest(administration).unwrap();
        assert_eq!(before, w.snapshot());
        let mut b = boundary(owner, 1);
        b.kind = BoundaryKind::PrecisePreemption;
        b.requested_target = Some(BranchCount(1));
        b.actual_target = Some(BranchCount(1));
        b.overshoot = Some(BranchCount(0));
        assert_eq!(
            w.import_stopped_guest(b).unwrap_err(),
            "precise-preemption contract unsatisfied"
        );
        assert_eq!(w.now(), 0);
    }
    #[test]
    fn completed_old_guest_response_remains_owned_after_reboot() {
        let mut w = world();
        let old = w.owner(MachineId(1));
        w.block(old, BlockOp::Read { offset: 0, len: 1 }, 0)
            .unwrap();
        w.step();
        w.step();
        w.crash(MachineId(1));
        w.reboot(MachineId(1));
        assert_eq!(
            w.completion(old.request).unwrap().kind,
            CompletionKind::Obsolete
        );
        assert!(w.import_stopped_guest(boundary(old, 0)).is_err());
    }
}

#[cfg(test)]
mod controller_boundary_test {
    use super::*;
    #[test]
    fn scenario_vm_progress_uses_an_immune_identity_not_a_workload_machine() {
        let mut w = World::new(42, &[(MachineId(1), 1)], 8, 8, Defects::default());
        w.register_controller(ControllerId(1), 1).unwrap();
        let b = StoppedGuestBoundary {
            owner: GuestOwner::ScenarioVm {
                controller: ControllerId(1),
                incarnation: 1,
            },
            boundary_sequence: 1,
            rip: 0x1000,
            rep: None,
            kind: BoundaryKind::Blocked,
            counter_origin: CounterOrigin::ScriptedModel,
            raw_counter: None,
            accumulated_guest_branches: BranchCount(7),
            delta_guest_branches: BranchCount(7),
            requested_target: None,
            actual_target: None,
            overshoot: None,
            pending_io_completed: true,
            tick_before: Tick(0),
            tick_after: Tick(7),
        };
        w.import_stopped_guest(b).unwrap();
        w.crash(MachineId(1));
        assert_eq!(w.controller_boundaries.len(), 1);
        assert_eq!(w.now(), 7);
        assert!(World::decode_state(&w.encode_state().unwrap()).is_ok());
    }
}
