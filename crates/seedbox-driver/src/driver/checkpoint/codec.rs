//! Version 8: fixed header then closed, field-ordered binary records.
//! Contains only continuing state; observation sessions are never encoded.
use super::*;
use crate::persistence::{
    require, CodecError, DecodeLimits, Decoder, Encoder, Result, SnapshotReference, Wire,
};
use std::io::{Read, Write};
const MAGIC: [u8; 8] = *b"SBXDRVR\0";
pub(super) const VERSION: u32 = 8;

impl<S> Checkpoint<S> {
    /// Consume this capture, replacing opaque snapshots in insertion order.
    /// All other owned state moves unchanged; no Clone/serde bound is needed.
    /// On Err this capture is consumed: completed replacements and remaining
    /// snapshots drop before return. The independent source Driver is unchanged.
    /// Artifact publication/cleanup from callbacks remains the caller's duty.
    // Consuming avoids cloning large snapshot archives while letting backend
    // codecs live outside the Driver without exposing its state.
    pub fn try_map_snapshots<T, E>(
        self,
        mut map: impl FnMut(S) -> std::result::Result<T, E>,
    ) -> std::result::Result<Checkpoint<T>, E> {
        let Self {
            now,
            rng,
            quantum_ns,
            quantum_end,
            machines,
            events,
            boundary_index,
            completed_quanta,
            quantum_committed,
            stopped,
            playback,
            oracle,
            finished,
            oracle_stops,
            control,
            inputs,
            trigger_marker,
            definitions,
            network,
            properties,
        } = self;
        let machines = machines
            .into_iter()
            .map(|entry| {
                let Entry {
                    machine,
                    rng,
                    lifecycle,
                    time,
                    position,
                    timer,
                    reached,
                    stopped,
                    inbox,
                    staged,
                    last_ended_published,
                } = entry;
                let machine = map(machine.expect("opaque stopped capture"))?;
                Ok(Entry {
                    machine: Some(machine),
                    rng,
                    lifecycle,
                    time,
                    position,
                    timer,
                    reached,
                    stopped,
                    inbox,
                    staged,
                    last_ended_published,
                })
            })
            .collect::<std::result::Result<_, E>>()?;
        Ok(Checkpoint {
            now,
            rng,
            quantum_ns,
            quantum_end,
            machines,
            events,
            boundary_index,
            completed_quanta,
            quantum_committed,
            stopped,
            playback,
            oracle,
            finished,
            oracle_stops,
            control,
            inputs,
            trigger_marker,
            definitions,
            network,
            properties,
        })
    }
}
struct Count(u64);
impl Write for Count {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(b.len() as u64)
            .ok_or_else(|| std::io::Error::other("checkpoint encoded length overflow"))?;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Checkpoint<SnapshotReference> {
    /// Stream one complete versioned Driver artifact, including future archive
    /// and disk bodies. Snapshot references are not resolved. Uses a counting
    /// pass then borrowed writes, never a whole-artifact buffer. A writer error
    /// may leave a partial destination; publish only after complete success.
    /// No simulation execution, random draws or source mutation occurs.
    /// Prefer a buffered writer for file/socket destinations: individual fields
    /// can cause small writes. Flushing (and durable/transactional publication)
    /// is the caller's responsibility; this method does not flush the writer.
    // Only the Driver can encode its private continuation fields and future
    // definitions. Raw framed bodies avoid JSON byte arrays and extra blob copies.
    pub fn write_to(&self, writer: &mut impl Write) -> Result<()> {
        self.validate_decoded()?;
        let mut count = Encoder { output: Count(0) };
        self.put(&mut count)?;
        writer.write_all(&MAGIC)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&count.output.0.to_le_bytes())?;
        self.put(&mut Encoder { output: writer })
    }
    /// Decode exactly one Driver artifact through EOF under explicit logical
    /// budgets. All fields/definitions are owned and validated before return;
    /// no worker, context, factory, backend decoder or native resource is used.
    /// Future unsupported RAM/Launch values remain deferred factory inputs.
    /// Ref digest/length resolution and external factory/service-history authority
    /// remain the adapter's obligations; this is not a complete service export.
    /// Prefer a buffered reader for file/socket sources. This method requires
    /// EOF after the artifact; for an enclosing stream, pass a `Read::take`
    /// boundary of the exact artifact length (buffer that bounded reader as
    /// appropriate), rather than exposing the next frame to the EOF check.
    /// Verify any artifact digest against original encoded bytes: decoding and
    /// re-encoding preserve semantics, not necessarily canonical input bytes.
    // Typed capture validation is insufficient for imported bytes, so this
    // closed bounded codec validates before any restore callbacks.
    pub fn read_from(reader: &mut impl Read, limits: DecodeLimits) -> Result<Self> {
        require(
            limits.encoded_bytes >= 20,
            "encoded budget smaller than header",
        )?;
        let mut d = Decoder::new(reader, limits.encoded_bytes, limits);
        require(<[u8; 8]>::get(&mut d)? == MAGIC, "magic")?;
        require(u32::get(&mut d)? == VERSION, "checkpoint version was made by an older or incompatible engine; replay with its retained engine or run fresh")?;
        let length = u64::get(&mut d)?;
        if length > d.remaining {
            return Err(CodecError::Limit("encoded bytes"));
        }
        d.remaining = length;
        let value = Self::get(&mut d)?;
        require(d.remaining == 0, "trailing payload")?;
        let mut extra = [0u8];
        require(d.input.read(&mut extra)? == 0, "trailing object bytes")?;
        let mut scratch = value.control.scratch_persist(&value.definitions)?;
        scratch = scratch
            .checked_add(value.network.scratch_persist()?)
            .ok_or(CodecError::Limit("scratch overflow"))?;
        for key in value.inputs.keys() {
            scratch = scratch
                .checked_add(key.as_str().len() as u64 + std::mem::size_of::<InputKey>() as u64)
                .ok_or(CodecError::Limit("scratch overflow"))?;
        }
        let metadata_scratch = d
            .allocation
            .checked_sub(d.bulk)
            .and_then(|n| n.checked_mul(2))
            .ok_or(CodecError::Limit("scratch overflow"))?;
        d.validation_scratch(scratch.max(metadata_scratch))?;
        value.validate_decoded()?;
        Ok(value)
    }
    fn check_action(&self, action: &Action, at: Timestamp) -> Result<()> {
        if let Some(target) = action.target() {
            match target {
                Target::Machine(id) => require(id.0 < self.machines.len(), "input target index")?,
                Target::Name(name) => {
                    require(self.control.target(name).is_ok(), "input target name")?
                }
            }
        }
        if let Action::Partition { left, right }
        | Action::Shape {
            from: left,
            to: right,
            ..
        } = action
        {
            for target in left.iter().chain(right) {
                match target {
                    Target::Machine(id) => {
                        require(id.0 < self.machines.len(), "partition input index")?
                    }
                    Target::Name(name) => require(
                        self.control.declared(name) || self.control.target(name).is_ok(),
                        "partition input name",
                    )?,
                }
            }
        }
        if let Action::Shape {
            policy,
            network_faults,
            ..
        } = action
        {
            policy
                .validate_shape(self.network.latency_ns(), network_faults)
                .map_err(|_| CodecError::Invalid("input shape policy"))?;
        }
        if let Action::DropConnection { connection: c } = action {
            require(
                c.from != c.to && c.src_port != 0 && c.dst_port != 0 && c.incarnation != 0,
                "input connection",
            )?;
            for name in [&c.from, &c.to] {
                require(
                    self.control.declared(name) || self.control.target(name).is_ok(),
                    "input connection member",
                )?;
            }
        }
        if let Some(deadline) = action.deadline() {
            require(
                u64::try_from(deadline.as_nanos())
                    .ok()
                    .filter(|n| *n > 0)
                    .and_then(|n| at.as_nanos().checked_add(n))
                    .is_some(),
                "input deadline",
            )?;
        }
        if let Some(operation) = action.bounded_clock_operation() {
            operation
                .validate_at(at.as_nanos())
                .map_err(|_| CodecError::Invalid("input bounded clock"))?;
        }
        if let Action::StepRealtime { nanoseconds, .. } = action {
            require(
                nanoseconds.get().unsigned_abs() <= crate::oracle::MAX_CLOCK_OFFSET_NS,
                "input realtime step",
            )?;
        }
        Ok(())
    }
    fn check_record(&self, r: &FaultRecord) -> Result<()> {
        require(
            self.control.service.token_persist(r.fault.0) && r.requested_at <= self.now,
            "fault record token/time",
        )?;
        match r.origin {
            FaultOrigin::Control { machine, .. } => {
                require(machine.0 < self.machines.len(), "fault caller")?
            }
            FaultOrigin::Trigger { trigger } => require(
                self.control.service.trigger_persist(trigger),
                "fault trigger",
            )?,
            FaultOrigin::Input { .. } => {}
        }
        match &r.target {
            FaultTarget::Machine { machine, boot } => {
                require(machine.0 < self.control.slots_persist()?, "fault target")?;
                if machine.0 < self.machines.len() {
                    require(
                        *boot <= self.machines[machine.0].lifecycle.boot(),
                        "fault target boot",
                    )?;
                } else {
                    require(*boot == 0, "reserved spawn boot")?;
                }
            }
            FaultTarget::Connection { connection } => require(
                self.network.connection_members(connection),
                "fault connection",
            )?,
            FaultTarget::DirectedLinks {
                from: left,
                to: right,
            }
            | FaultTarget::Links { left, right } => {
                require(
                    !left.is_empty()
                        && !right.is_empty()
                        && left.windows(2).all(|w| w[0] < w[1])
                        && right.windows(2).all(|w| w[0] < w[1]),
                    "fault link sets",
                )?;
                for id in left.iter().chain(right) {
                    require(
                        id.0 < self.machines.len() && self.network.attached(*id),
                        "fault link member",
                    )?;
                }
                require(
                    left.iter().all(|id| !right.contains(id)),
                    "overlapping fault links",
                )?;
            }
        }
        require(
            matches!(
                (&r.action, &r.target),
                (FaultAction::Partition, FaultTarget::Links { .. })
                    | (FaultAction::Shape { .. }, FaultTarget::DirectedLinks { .. })
                    | (FaultAction::DropConnection, FaultTarget::Connection { .. })
                    | (FaultAction::Heal, _)
                    | (
                        FaultAction::Spawn
                            | FaultAction::Pause
                            | FaultAction::Resume
                            | FaultAction::Crash
                            | FaultAction::Reboot
                            | FaultAction::Shutdown
                            | FaultAction::SetExecution(_)
                            | FaultAction::Application(_)
                            | FaultAction::Drift(_)
                            | FaultAction::StepRealtime(_)
                            | FaultAction::DeviceFault(_),
                        FaultTarget::Machine { .. },
                    )
            ),
            "fault action/target",
        )?;
        if let FaultAction::Shape {
            policy,
            network_faults,
        } = r.action
        {
            policy
                .validate_shape(self.network.latency_ns(), &network_faults)
                .map_err(|_| CodecError::Invalid("fault shape policy"))?;
        }
        Ok(())
    }
    fn check_retired_outputs(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        let mut visit = |output: &MachineResponse| -> Result<()> {
            let Some(boot) = boot_end::shape(&output.payload).map_err(CodecError::Invalid)? else {
                return Ok(());
            };
            let entry = self
                .machines
                .get(output.machine.0)
                .ok_or(CodecError::Invalid("retired output machine"))?;
            require(
                boot <= entry.lifecycle.boot() && boot <= output.boot,
                "retired output ordinal",
            )?;
            require(
                entry.last_ended_published.is_some_and(|last| boot <= last),
                "retired output lacks cursor",
            )?;
            require(
                seen.insert((output.machine, boot)),
                "duplicate retained boot-end output",
            )
        };
        for entry in &self.machines {
            for output in &entry.staged {
                visit(output)?;
            }
        }
        Ok(())
    }
    fn validate_decoded(&self) -> Result<()> {
        validate(
            self.now,
            self.quantum_ns,
            self.quantum_end,
            self.oracle,
            &self.machines,
            &self.control,
            &self.network,
        )
        .map_err(CodecError::Invalid)?;
        self.rng
            .check_persist(crate::rng::RngDomain::Machine, None)?;
        require(
            self.completed_quanta <= self.boundary_index
                && self.quantum_committed == (self.now == self.quantum_end)
                && (!self.quantum_committed || self.completed_quanta > 0),
            "quantum counters/position",
        )?;
        let ends = u128::from(self.completed_quanta) + u128::from(!self.quantum_committed);
        let elapsed = ends
            .checked_mul(u128::from(self.quantum_ns))
            .ok_or(CodecError::Invalid("quantum counter overflow"))?;
        require(
            elapsed <= u128::from(self.quantum_end.as_nanos())
                && u128::from(self.now.as_nanos())
                    >= u128::from(self.quantum_end.as_nanos()).saturating_sub(elapsed),
            "quantum anchor",
        )?;
        self.network
            .check_persist(self.machines.len(), self.now.as_nanos(), &self.definitions)?;
        self.rng.check_admissions_persist(
            (0..self.machines.len()).map(|i| self.network.member_seed(i)),
        )?;
        require(
            self.control
                .controller
                .and_then(|setup| setup.controller)
                .is_none_or(|id| id == self.oracle),
            "controller must be oracle",
        )?;
        self.control.check_persist(
            &self.machines,
            &self.definitions,
            &self.network,
            self.now.as_nanos(),
        )?;
        for (id, entry) in self.machines.iter().enumerate() {
            entry
                .rng
                .as_ref()
                .ok_or(CodecError::Invalid("missing machine RNG"))?
                .check_persist(
                    crate::rng::RngDomain::Machine,
                    Some(self.network.member_seed(id)),
                )?;
            entry
                .time
                .check_persist(self.now.as_nanos(), entry.position.as_nanos())?;
            entry.lifecycle.check_persist(
                self.now.as_nanos(),
                &control::Control::source(MachineId(id)),
            )?;
            for input in &entry.inbox {
                match &input.payload {
                    Request::ReceiveFrame { bytes } => {
                        require(crate::pv::frame_in_bounds(bytes), "retained frame")?
                    }
                    Request::StepRealtime { nanoseconds, .. } => require(
                        nanoseconds.get().unsigned_abs() <= crate::oracle::MAX_CLOCK_OFFSET_NS,
                        "retained realtime step",
                    )?,
                    _ => {}
                }
            }
            for output in &entry.staged {
                require(
                    output.machine == MachineId(id)
                        && output.at <= self.now
                        && output.boot <= entry.lifecycle.boot(),
                    "staged output identity/time",
                )?;
            }
        }
        let mut pending = BTreeSet::new();
        let mut queued_calls = BTreeSet::new();
        let mut queued_replies = BTreeSet::new();
        for (at, queue) in &self.events {
            require(
                *at >= self.now && !queue.is_empty(),
                "scheduled event time/empty queue",
            )?;
            for event in queue {
                match event {
                    Scheduled::Driver(
                        DriverEvent::Pause(id)
                        | DriverEvent::Resume(id)
                        | DriverEvent::SetExecution { machine: id, .. }
                        | DriverEvent::SetClockRate { machine: id, .. },
                    )
                    | Scheduled::Input { id, .. } => {
                        require(id.0 < self.machines.len(), "scheduled machine index")?
                    }
                    Scheduled::PropertyReply {
                        machine,
                        boot,
                        payload,
                        ..
                    } => require(
                        machine.0 < self.machines.len()
                            && *boot <= self.machines[machine.0].lifecycle.boot()
                            && serde_json::from_slice::<seedbox_engine_protocol::property::Reply>(
                                payload,
                            )
                            .is_ok(),
                        "scheduled property reply",
                    )?,
                    Scheduled::Control(control::Event::Call {
                        source,
                        boot,
                        observed,
                        id,
                        ..
                    }) => {
                        require(source.0 < self.machines.len(), "call source")?;
                        require(
                            queued_calls.insert((
                                *source,
                                *boot,
                                id.session.get(),
                                id.request.get(),
                            )),
                            "duplicate queued call",
                        )?;
                        require(
                            *boot <= self.machines[source.0].lifecycle.boot()
                                && *observed <= self.now.as_nanos()
                                && *observed <= at.as_nanos(),
                            "call time/boot",
                        )?;
                        if let Some(setup) = self.control.controller {
                            require(
                                setup.controller == Some(*source)
                                    && observed.checked_add(setup.latency_ns)
                                        == Some(at.as_nanos()),
                                "controller call authority/time",
                            )?;
                        }
                    }
                    Scheduled::Control(control::Event::Reply {
                        target,
                        boot,
                        id,
                        payload,
                    }) => {
                        require(
                            target.0 < self.machines.len()
                                && *boot <= self.machines[target.0].lifecycle.boot(),
                            "reply boot identity",
                        )?;
                        require(
                            queued_replies.insert((
                                control::Control::source(*target),
                                id.session.get(),
                                id.request.get(),
                                *boot,
                            )),
                            "duplicate queued reply",
                        )?;
                        self.control.check_reply_persist(
                            *target,
                            *id,
                            payload,
                            at.as_nanos(),
                            self.now.as_nanos(),
                        )?;
                    }
                    Scheduled::Control(control::Event::Plan(plan)) => {
                        self.control.check_plan_persist(plan, at.as_nanos())?;
                        if let crate::oracle::service::Plan::Reply { source, id, .. } = plan {
                            require(
                                queued_replies.insert((
                                    source.clone(),
                                    id.session.get(),
                                    id.request.get(),
                                    self.control.calls_boot_persist(source, *id)?,
                                )),
                                "duplicate queued reply",
                            )?;
                        }
                    }
                    Scheduled::Control(control::Event::Input {
                        key,
                        action,
                        handle,
                    }) => {
                        self.check_action(action, *at)?;
                        require(
                            handle.is_none() || action.installs_handle(),
                            "name on handleless input",
                        )?;
                        require(pending.insert(key.clone()), "duplicate queued input")?;
                        require(
                            matches!(self.inputs.get(key), Some(LiveInput::Pending)),
                            "queued input lacks pending key",
                        )?;
                    }
                    _ => {}
                }
            }
        }
        let mut owners = BTreeSet::new();
        for (key, state) in &self.inputs {
            match state {
                LiveInput::Pending => {
                    require(pending.contains(key), "pending key lacks queued input")?
                }
                LiveInput::Admitted { handle, record_id } => {
                    require(
                        handle.is_some() == record_id.is_some(),
                        "input record identity",
                    )?;
                    if let (Some(id), Some(text)) = (record_id, handle) {
                        require(owners.insert(*id), "duplicate input owner")?;
                        self.control
                            .service
                            .check_owned_input_persist(*id, text, false)?;
                    }
                }
                LiveInput::Handle(owned) => {
                    require(owners.insert(owned.record_id), "duplicate input owner")?;
                    self.control.service.check_owned_input_persist(
                        owned.record_id,
                        &owned.text,
                        true,
                    )?;
                }
            }
        }
        self.properties.validate().map_err(CodecError::Invalid)?;
        self.check_retired_outputs()?;
        self.playback.check_persist(self.completed_quanta)?;
        if let Some(held) = &self.playback.held {
            self.check_record(held)?;
            require(
                self.control.has_pending(held.fault),
                "held fault is not pending",
            )?;
            require(
                self.control
                    .pending_record(held.fault.0)
                    .is_ok_and(|r| &r == held),
                "held fault record",
            )?;
            require(
                matches!(self.events.get(&self.now).and_then(|q|q.front()),Some(Scheduled::Control(control::Event::Plan(crate::oracle::service::Plan::Effect{fault,..}))) if *fault==held.fault.0),
                "held fault is not FIFO front",
            )?;
        }
        Ok(())
    }
}
impl<T: Wire> Wire for Entry<T> {
    fn put<W: Write>(&self, e: &mut Encoder<W>) -> Result<()> {
        self.machine.put(e)?;
        self.rng.put(e)?;
        self.lifecycle.put(e)?;
        self.time.put(e)?;
        self.position.put(e)?;
        self.timer.put(e)?;
        self.reached.put(e)?;
        self.stopped.put(e)?;
        self.inbox.put(e)?;
        self.staged.put(e)?;
        self.last_ended_published.put(e)
    }
    fn get<R: Read>(d: &mut Decoder<R>) -> Result<Self> {
        d.nested(|d| {
            Ok(Self {
                machine: Wire::get(d)?,
                rng: Wire::get(d)?,
                lifecycle: Wire::get(d)?,
                time: Wire::get(d)?,
                position: Wire::get(d)?,
                timer: Wire::get(d)?,
                reached: Wire::get(d)?,
                stopped: Wire::get(d)?,
                inbox: Wire::get(d)?,
                staged: Wire::get(d)?,
                last_ended_published: Wire::get(d)?,
            })
        })
    }
}
wire_struct!(Checkpoint<SnapshotReference>{now,rng,quantum_ns,quantum_end,machines,events,boundary_index,completed_quanta,quantum_committed,stopped,playback,oracle,finished,oracle_stops,control,inputs,trigger_marker,definitions,network,properties});
