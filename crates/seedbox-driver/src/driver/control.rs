//! Concrete private control ownership. Every machine has a canonical private
//! key (`machine:N`), which the service uses as its identity for targets,
//! handles, triggers and replies. Optional display names (which are also
//! service names) resolve guest and host name targets, never the reverse, so
//! unnamed machines and duplicate display names stay safely targetable by
//! identity.
use super::{Entry, MachineId, Timestamp};
use crate::{
    definition::MachineDefinition,
    oracle::{
        observation::{Origin, State},
        service::{Adapter, CommandKind, CommandOwner, Facts, Service},
        OperationError, Target,
    },
    pv::Request,
    rng::{RngDomain, RngStream, SeededRng},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

#[derive(Clone, Default)]
struct Commands {
    next: u32,
    used: BTreeMap<NonZeroU32, (CommandKind, Origin, bool)>,
}
/// The correlated guest command carried by a request, if any. Application
/// commands and realtime steps share one per-boot ID space.
pub(super) fn command(request: &Request) -> Option<(NonZeroU32, CommandKind)> {
    match *request {
        Request::Application { id, command } => Some((id, CommandKind::Application(command))),
        Request::StepRealtime { id, .. } => Some((id, CommandKind::StepRealtime)),
        _ => None,
    }
}
impl Commands {
    fn reserve(&mut self, fault: u32, action: CommandKind) -> Result<NonZeroU32, String> {
        loop {
            self.next = self
                .next
                .checked_add(1)
                .ok_or("application command IDs exhausted")?;
            let id = NonZeroU32::new(self.next).unwrap();
            if let std::collections::btree_map::Entry::Vacant(entry) = self.used.entry(id) {
                entry.insert((action, Origin::Service { fault }, false));
                return Ok(id);
            }
        }
    }
    fn reset(&mut self) {
        self.next = 0;
        self.used.clear();
    }
}
/// Control-owned facts of one machine. Its committed lifecycle record is
/// `Entry::lifecycle`, which the adapter reads.
#[derive(Clone)]
struct Observed {
    name: Option<String>,
    /// The definition attached a network interface; host additions never do.
    attached: bool,
    commands: Commands,
    /// Seeded clock-fault values for this target, derived from the machine
    /// seed in its own domain; survives reboot, independent of guest entropy.
    clock: SeededRng,
}
#[derive(Clone)]
pub(super) struct Control {
    pub(super) service: Service,
    pub(super) controller: Option<super::ControllerSetup>,
    entries: Vec<Observed>,
    // Removing a declared name at admission rejects same-barrier duplicates.
    spawnable: BTreeSet<String>,
    /// Declared spawnable names whose definitions attach an interface.
    attached_spawnable: BTreeSet<String>,
    // Reserved identities are not targetable until their FIFO construction turn.
    // Failure retains this association for its Failed fault record.
    spawning: BTreeMap<String, MachineId>,
    /// Boot of each admitted call awaiting its reply; live bookkeeping only.
    calls: BTreeMap<(MachineId, u32, u32), u64>,
}
impl Control {
    pub(super) fn machine_names(&self) -> impl ExactSizeIterator<Item = (MachineId, Option<&str>)> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (MachineId(index), entry.name.as_deref()))
    }

    pub(super) fn checkpoint_size(&self, machines: usize) -> bool {
        self.entries.len() == machines
    }

    /// `seeds` are the machine seeds already assigned in identity order.
    pub(super) fn new(definitions: &[MachineDefinition], seeds: &[u64]) -> Self {
        let entries = definitions
            .iter()
            .zip(seeds)
            .map(|(definition, &seed)| Observed {
                name: Some(definition.name.clone()),
                attached: definition.network.is_some(),
                commands: Commands::default(),
                clock: SeededRng::derived(seed, RngDomain::ClockFaults),
            })
            .collect();
        Self {
            attached_spawnable: BTreeSet::new(),
            service: Service::default(),
            controller: None,
            calls: BTreeMap::new(),
            entries,
            spawnable: BTreeSet::new(),
            spawning: BTreeMap::new(),
        }
    }
    pub(super) fn declare_spawnable(&mut self, definitions: &[MachineDefinition]) {
        self.spawnable
            .extend(definitions.iter().map(|d| d.name.clone()));
        self.attached_spawnable.extend(
            definitions
                .iter()
                .filter(|d| d.network.is_some())
                .map(|d| d.name.clone()),
        );
    }
    pub(super) fn spawned(&mut self, name: &str, seed: u64) -> Result<MachineId, String> {
        let id = *self.spawning.get(name).ok_or("unreserved spawn")?;
        if id.0 != self.entries.len() {
            return Err("spawn identity out of insertion order".into());
        }
        self.entries.push(Observed {
            name: Some(name.into()),
            attached: self.attached_spawnable.contains(name),
            commands: Commands::default(),
            clock: SeededRng::derived(seed, RngDomain::ClockFaults),
        });
        self.spawning.remove(name);
        Ok(id)
    }
    pub(super) fn spawn_target(&self, name: &str) -> Result<MachineId, String> {
        self.spawning
            .get(name)
            .copied()
            .map(Ok)
            .unwrap_or_else(|| self.target(name).map_err(|e| e.message))
    }
    /// Whether a declared name can still be admitted as a spawn.
    pub(super) fn spawnable(&self, name: &str) -> bool {
        self.spawnable.contains(name)
    }
    /// A declared spawnable name, whether or not its spawn is reserved.
    pub(super) fn declared(&self, name: &str) -> bool {
        self.spawnable.contains(name) || self.spawning.contains_key(name)
    }
    /// Describe a pending fault as it was admitted, for a pre-effect hold.
    pub(super) fn pending_record(&self, fault: u32) -> Result<super::FaultRecord, String> {
        let a = self
            .service
            .pending_admitted(fault)
            .ok_or("held fault is not pending")?;
        self.record(
            a.fault,
            &a.requester,
            &a.scope,
            &a.action,
            &a.handle,
            a.requested_at,
        )
    }
    pub(super) fn spawn_pending(&self) -> bool {
        !self.spawning.is_empty()
    }
    pub(super) fn add_unnamed(&mut self, seed: u64) {
        self.entries.push(Observed {
            name: None,
            attached: false,
            commands: Commands::default(),
            clock: SeededRng::derived(seed, RngDomain::ClockFaults),
        });
    }
    /// Canonical private key of a machine, also its control-call source.
    pub(super) fn source(id: MachineId) -> String {
        format!("machine:{}", id.0)
    }
    fn key_index(entries: &[Observed], key: &str) -> Result<usize, String> {
        let index = key
            .strip_prefix("machine:")
            .and_then(|index| index.parse::<usize>().ok())
            .filter(|index| Self::source(MachineId(*index)) == key)
            .ok_or("invalid private machine key")?;
        if index >= entries.len() {
            return Err("unknown private machine key".into());
        }
        Ok(index)
    }
    fn name_index(entries: &[Observed], name: &str) -> Result<usize, String> {
        let mut matches = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.as_deref() == Some(name));
        let Some((index, _)) = matches.next() else {
            return Err("unknown machine target".into());
        };
        if matches.next().is_some() {
            return Err("ambiguous machine target".into());
        }
        Ok(index)
    }
    /// Resolve a canonical key, such as a call source or a plan target.
    pub(super) fn reply_target(&self, key: &str) -> Result<MachineId, String> {
        Self::key_index(&self.entries, key).map(MachineId)
    }
    /// Resolve a display (service) name that identifies exactly one machine.
    pub(super) fn target(&self, name: &str) -> Result<MachineId, OperationError> {
        Self::name_index(&self.entries, name)
            .map(MachineId)
            .map_err(OperationError::invalid)
    }
    pub(super) fn offered(
        &mut self,
        id: MachineId,
        command: NonZeroU32,
        action: CommandKind,
        origin: Origin,
    ) -> Result<(), String> {
        let used = &mut self.entries[id.0].commands.used;
        if let Some(&(reserved_action, reserved_origin, _)) = used.get(&command) {
            if matches!(origin, Origin::Service { .. })
                && reserved_action == action
                && reserved_origin == origin
            {
                return Ok(());
            }
            return Err("application command ID already reserved in this boot".into());
        }
        used.insert(command, (action, origin, false));
        Ok(())
    }
    pub(super) fn accepted(
        &mut self,
        id: MachineId,
        lifecycle: &mut State,
        boot: u64,
        command: NonZeroU32,
        at: Timestamp,
        commit: Timestamp,
    ) -> Result<(), String> {
        let (action, origin, accepted) = self.entries[id.0]
            .commands
            .used
            .get_mut(&command)
            .ok_or("unreserved application command")?;
        if !*accepted {
            lifecycle
                .command_admitted(
                    boot,
                    command,
                    *action,
                    *origin,
                    at.as_nanos(),
                    commit.as_nanos(),
                )
                .map_err(|e| format!("command admission: {e:?}"))?;
            *accepted = true;
        }
        Ok(())
    }
    pub(super) fn reset_commands(&mut self, id: MachineId) {
        self.entries[id.0].commands.reset();
    }
}
pub(super) struct View<'a, M> {
    pub(super) guest_controller: Option<MachineId>,
    network: Option<&'a super::network::Network>,
    entries: &'a mut [Observed],
    machines: &'a [Entry<M>],
    spawnable: &'a mut BTreeSet<String>,
    spawning: &'a mut BTreeMap<String, MachineId>,
}
impl<M> View<'_, M> {
    fn index(&self, key: &str) -> Result<usize, String> {
        Control::key_index(self.entries, key)
    }
    fn lifecycle(&self, index: usize) -> Result<&State, String> {
        self.machines
            .get(index)
            .map(|entry| &entry.lifecycle)
            .ok_or_else(|| "machine lacks a lifecycle record".into())
    }
}
impl<M> Adapter for View<'_, M> {
    fn check_control_target(&self, target: &Target) -> Result<(), OperationError> {
        let Some(controller) = self.guest_controller else {
            return Ok(());
        };
        let index = match target {
            Target::Machine(name) | Target::Service(name) => {
                Control::name_index(self.entries, name)
            }
            Target::Key(key) => self.index(key),
        }
        .map_err(OperationError::invalid)?;
        if controller == MachineId(index) {
            return Err(OperationError::invalid(
                "controller is outside failure model",
            ));
        }
        Ok(())
    }
    fn draw(&mut self, target: &str) -> Result<u64, OperationError> {
        let index = self.index(target).map_err(OperationError::invalid)?;
        let mut bytes = [0; 8];
        self.entries[index].clock.fill_bytes(&mut bytes);
        Ok(u64::from_le_bytes(bytes))
    }
    fn reserve_spawn(&mut self, name: &str) -> Result<(), OperationError> {
        if !self.spawnable.remove(name) {
            return Err(OperationError::invalid("unknown or already admitted spawn"));
        }
        let id = MachineId(self.entries.len() + self.spawning.len());
        self.spawning.insert(name.into(), id);
        Ok(())
    }
    /// A service name is its machine's name, so both wire forms resolve the
    /// same display name; internal re-resolution uses canonical keys.
    fn resolve(&self, target: &Target) -> Result<Facts, OperationError> {
        let index = match target {
            Target::Machine(name) | Target::Service(name) => {
                Control::name_index(self.entries, name)
            }
            Target::Key(key) => self.index(key),
        }
        .map_err(OperationError::invalid)?;
        if self.guest_controller == Some(MachineId(index)) {
            return Err(OperationError::invalid(
                "controller is outside failure model",
            ));
        }
        self.lifecycle(index)
            .map_err(OperationError::invalid)?
            .facts()
            .map_err(|_| OperationError::invalid("unavailable observation"))
    }
    fn network_member(&self, target: &Target) -> Result<String, OperationError> {
        let index = match target {
            Target::Machine(name) | Target::Service(name) => {
                Control::name_index(self.entries, name).map_err(|message| {
                    if self.spawnable.contains(name) || self.spawning.contains_key(name) {
                        OperationError {
                            code: crate::oracle::ErrorCode::TargetUnavailable,
                            message: "partition member is not spawned".into(),
                            handle: None,
                        }
                    } else {
                        OperationError::invalid(message)
                    }
                })?
            }
            Target::Key(key) => self.index(key).map_err(OperationError::invalid)?,
        };
        if self.guest_controller == Some(MachineId(index)) {
            return Err(OperationError::invalid(
                "controller is outside failure model",
            ));
        }
        if !self.entries[index].attached {
            return Err(OperationError::invalid(
                "partition member has no network interface",
            ));
        }
        Ok(Control::source(MachineId(index)))
    }
    fn network_floor(&self) -> Result<u64, OperationError> {
        self.network
            .map(|n| n.latency_ns())
            .ok_or_else(|| OperationError::invalid("network view missing"))
    }
    fn connections(
        &self,
        from: &str,
        to: &str,
        after: u64,
    ) -> Result<Vec<crate::driver::Connection>, OperationError> {
        let name = |key: &str| {
            let i = self.index(key).map_err(OperationError::invalid)?;
            self.entries[i]
                .name
                .as_deref()
                .ok_or_else(|| OperationError::invalid("network member has no prepared name"))
        };
        Ok(self
            .network
            .ok_or_else(|| OperationError::invalid("network view missing"))?
            .connections(name(from)?, name(to)?, after))
    }
    fn selection_available(&self, c: &crate::driver::Connection) -> Result<bool, OperationError> {
        Ok(self
            .network
            .ok_or_else(|| OperationError::invalid("network view missing"))?
            .selection_available(c))
    }
    fn selection_counts(
        &self,
        handle: &str,
    ) -> Result<Option<crate::driver::NetworkSettlement>, OperationError> {
        Ok(self
            .network
            .ok_or_else(|| OperationError::invalid("network view missing"))?
            .selection_counts(handle))
    }

    fn command_id(
        &mut self,
        target: &str,
        boot: u64,
        fault: u32,
        action: CommandKind,
    ) -> Result<NonZeroU32, String> {
        let index = self.index(target)?;
        let lifecycle = self.lifecycle(index)?;
        if lifecycle
            .view()
            .map_err(|e| format!("observation: {e:?}"))?
            .boot
            != boot
        {
            return Err("command reservation for stale boot".into());
        }
        self.entries[index].commands.reserve(fault, action)
    }
    fn command_owner(
        &self,
        target: &str,
        boot: u64,
        id: NonZeroU32,
    ) -> Result<CommandOwner, String> {
        self.lifecycle(self.index(target)?)?
            .command_owner(boot, id)
            .map_err(|e| format!("command correlation: {e:?}"))
    }
    fn inspect(
        &self,
        handles: &[String],
        triggers: &[crate::oracle::triggers::Record],
    ) -> Result<Value, OperationError> {
        use crate::oracle::{
            service::{Process, Vm},
            triggers::{Status, TriggerAction},
        };
        let network = self
            .network
            .ok_or_else(|| OperationError::invalid("network view missing"))?;
        let display = |key: &str| -> Result<String, OperationError> {
            let i = self.index(key).map_err(OperationError::invalid)?;
            Ok(self.entries[i]
                .name
                .clone()
                .unwrap_or_else(|| Control::source(MachineId(i))))
        };
        let mut services = BTreeMap::new();
        let mut machines = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            let name = entry
                .name
                .clone()
                .unwrap_or_else(|| Control::source(MachineId(i)));
            services.insert(name.clone(), name.clone());
            let state = self.lifecycle(i).map_err(OperationError::invalid)?;
            let facts = state
                .facts()
                .map_err(|_| OperationError::invalid("unavailable observation"))?;
            let state = if facts.vm == Vm::Down {
                "Down".into()
            } else if facts.physically_ended {
                let view = state
                    .view()
                    .map_err(|_| OperationError::invalid("unavailable observation"))?;
                match view.first_terminal.map(|t| t.reason) {
                    Some(crate::machine::ExitReason::ProgramExit { code }) => {
                        format!("Exited({code})")
                    }
                    Some(crate::machine::ExitReason::Crashed) => "Down".into(),
                    _ => "Exited(0)".into(),
                }
            } else if facts.vm == Vm::Paused {
                "Paused".into()
            } else {
                match facts.process {
                    Process::Running => "Running",
                    Process::Stopped => "Stopped",
                    Process::Frozen => "Frozen",
                }
                .into()
            };
            machines.push(serde_json::json!({"name":name,"state":state}));
        }
        let mut result = serde_json::json!({"services":services,"machines":machines,"active_handles":handles,"network_counters":network.legacy_counts()});
        if !triggers.is_empty() {
            let mut records = Vec::new();
            for r in triggers {
                let action = match &r.action {
                    TriggerAction::Stop(name) => {
                        serde_json::json!({"action":"stop","machine":display(name)?})
                    }
                    TriggerAction::Partition { left, right } => {
                        let side = |names: &[String]| {
                            names
                                .iter()
                                .map(|n| display(n))
                                .collect::<Result<Vec<_>, _>>()
                        };
                        serde_json::json!({"action":"partition","left":side(left)?,"right":side(right)?})
                    }
                };
                let status = match r.status {
                    Status::Armed => "armed",
                    Status::Cancelled => "cancelled",
                    Status::Matched => "matched",
                    Status::Expired => "expired",
                    Status::Rejected => "rejected",
                    Status::Fired => "fired",
                    Status::CutOff => "cut_off",
                };
                let mut record = serde_json::json!({"id":r.id,"source":display(&r.source)?,"marker":r.marker,"armed_at_ns":r.armed_at,"match_deadline_ns":r.match_deadline,"action":action,"prefix":r.prefix,"seen":r.seen,"resets":r.resets,"matched_at_ns":r.matched_at,"effect_at_ns":r.effect_at,"fault_id":r.fault_id,"cancelled_at_ns":r.cancelled_at,"cut_off_at_ns":r.cut_off_at,"status":status});
                // A trigger has no fault handle until ordinary admission at
                // its effect. The retained handle is independent of fault_id.
                if let Some(handle) = &r.handle {
                    record["handle"] = serde_json::json!(handle);
                }
                if let Some(reason) = &r.rejection_reason {
                    record["rejection_reason"] = serde_json::json!(reason);
                }
                records.push(record);
            }
            result["triggers"] = serde_json::json!(records);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;

/// Events share the Driver queue, including zero-time calls and expiry markers.
#[derive(Clone)]
pub(super) enum Event {
    Call {
        source: MachineId,
        boot: u64,
        id: crate::pv::SimulationControlId,
        observed: u64,
        payload: Vec<u8>,
    },
    /// Delayed service reply, pinned to the caller boot at plan publication.
    Reply {
        target: MachineId,
        boot: u64,
        id: crate::pv::SimulationControlId,
        payload: Vec<u8>,
    },
    Plan(crate::oracle::service::Plan),
    /// A host input, admitted through the control policy when it is due.
    Input {
        key: super::InputKey,
        action: super::Action,
        handle: Option<super::FaultHandle>,
    },
    TriggerBoundary,
    TriggerFinalize,
}
impl Control {
    pub(super) fn invoke<T, M>(
        &mut self,
        machines: &[Entry<M>],
        f: impl FnOnce(&mut Service, &mut View<'_, M>) -> Result<T, crate::oracle::service::Failure>,
    ) -> Result<T, String> {
        self.invoke_with_network(machines, None, f)
    }
    pub(super) fn invoke_with_network<T, M>(
        &mut self,
        machines: &[Entry<M>],
        network: Option<&super::network::Network>,
        f: impl FnOnce(&mut Service, &mut View<'_, M>) -> Result<T, crate::oracle::service::Failure>,
    ) -> Result<T, String> {
        let result = f(
            &mut self.service,
            &mut View {
                guest_controller: None,
                network,
                entries: &mut self.entries,
                machines,
                spawnable: &mut self.spawnable,
                spawning: &mut self.spawning,
            },
        );
        // Settlements in a Failure were already recorded as service notices,
        // which the Driver publishes even when this call fails.
        result.map_err(|failure| failure.message)
    }
    /// Settlement plans remain solely so the queue owner can retire their
    /// already-scheduled deadlines; their facts publish through notices.
    pub(super) fn invoke_plans<M>(
        &mut self,
        machines: &[Entry<M>],
        f: impl FnOnce(
            &mut Service,
            &mut View<'_, M>,
        )
            -> Result<Vec<crate::oracle::service::Plan>, crate::oracle::service::Failure>,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        self.invoke(machines, f)
    }
    /// The service's identity for this machine: its canonical key.
    pub(super) fn name(&self, id: MachineId) -> String {
        Self::source(id)
    }
    /// The single path for a confirmed Driver-applied lifecycle or setting
    /// effect: it retires the ended boot's control work, then commits the
    /// transition to the machine's lifecycle record.
    pub(super) fn admission<M>(
        &mut self,
        machines: &mut [Entry<M>],
        id: MachineId,
        at: Timestamp,
        transition: crate::oracle::observation::Transition,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        use crate::oracle::observation::{Admission, Transition};
        let boot = machines[id.0].lifecycle.boot();
        let mut plans = vec![];
        if matches!(transition, Transition::Crash | Transition::Reboot { .. }) {
            let name = self.name(id);
            plans = self.invoke_plans(machines, |service, _| {
                service.ended(&name, boot, at.as_nanos(), at.as_nanos())
            })?;
            self.service.source_reset(&name).map_err(|e| e.message)?;
        }
        machines[id.0]
            .lifecycle
            .admission(
                boot,
                at.as_nanos(),
                at.as_nanos(),
                Admission::Applied(transition),
            )
            .map_err(|e| format!("control observation admission: {e:?}"))?;
        if matches!(transition, Transition::Reboot { .. }) {
            self.reset_commands(id);
        }
        Ok(plans)
    }
    pub(super) fn observe<M>(
        &mut self,
        machines: &mut [Entry<M>],
        output: &super::MachineResponse,
        commit: Timestamp,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        use crate::pv::Response;
        let mut plans = vec![];
        let name = self.name(output.machine);
        match &output.payload {
            Response::CommandAcknowledged { id, result } => {
                plans.extend(self.invoke_plans(machines, |service, view| {
                    service.acknowledged(
                        view,
                        &name,
                        output.boot,
                        *id,
                        *result,
                        output.at.as_nanos(),
                        commit.as_nanos(),
                    )
                })?)
            }
            Response::ApplicationOutcome { .. } => {
                plans.extend(self.invoke_plans(machines, |service, _| {
                    service.application_ended(
                        &name,
                        output.boot,
                        output.at.as_nanos(),
                        commit.as_nanos(),
                    )
                })?)
            }
            Response::ApplicationOutput { bytes, .. } | Response::BootConsole { bytes } => self
                .service
                .committed_bytes(&name, output.at.as_nanos(), bytes, commit.as_nanos())
                .map_err(|e| e.message)?,
            _ => {}
        }
        machines[output.machine.0]
            .lifecycle
            .response(
                output.boot,
                output.at.as_nanos(),
                commit.as_nanos(),
                &output.payload,
            )
            .map_err(|e| format!("control observation response: {e:?}"))?;
        if matches!(output.payload, Response::Ready) {
            plans.extend(self.invoke_plans(machines, |service, view| {
                service.ready(
                    view,
                    &name,
                    output.boot,
                    output.at.as_nanos(),
                    commit.as_nanos(),
                )
            })?);
        }
        Ok(plans)
    }
    /// Fold an accepted physical exit into the committed record: only its first
    /// occurrence in a boot changes it, never a later adopted boundary.
    pub(super) fn terminal<M>(
        &mut self,
        machines: &mut [Entry<M>],
        id: MachineId,
        commit: Timestamp,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        let crate::machine::StopReason::Exited { reached, reason } = machines[id.0].stopped else {
            return Ok(vec![]);
        };
        let boot = machines[id.0].lifecycle.boot();
        let change = machines[id.0]
            .lifecycle
            .terminal(boot, reached.as_nanos(), commit.as_nanos(), reason)
            .map_err(|e| format!("control terminal: {e:?}"))?;
        if change != crate::oracle::observation::Change::Changed {
            return Ok(vec![]);
        }
        let name = self.name(id);
        self.invoke_plans(machines, |service, _| {
            service.ended(&name, boot, reached.as_nanos(), commit.as_nanos())
        })
    }
}

/// Conversion of private service notices into public events. Names were
/// resolved uniquely by the service, so they map back to one identity.
impl Control {
    pub(super) fn record_call(
        &mut self,
        source: MachineId,
        id: crate::pv::SimulationControlId,
        boot: u64,
    ) {
        self.calls
            .insert((source, id.session.get(), id.request.get()), boot);
    }
    /// Retire the call's bookkeeping and return the boot that made it.
    pub(super) fn take_call(
        &mut self,
        source: MachineId,
        id: crate::pv::SimulationControlId,
    ) -> Result<u64, String> {
        self.calls
            .remove(&(source, id.session.get(), id.request.get()))
            .ok_or_else(|| "reply for an unadmitted call".into())
    }
    fn named(&self, key: &str) -> Result<MachineId, String> {
        self.reply_target(key)
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record(
        &self,
        fault: u32,
        requester: &crate::oracle::service::Requester,
        scope: &crate::oracle::service::Scope,
        action: &crate::oracle::service::FaultAction,
        handle: &Option<String>,
        requested_at: u64,
    ) -> Result<super::FaultRecord, String> {
        use super::{FaultAction as Public, FaultOrigin};
        use crate::oracle::service::{Effect, FaultAction, Requester, Scope};
        Ok(super::FaultRecord {
            fault: super::FaultId(fault),
            origin: match requester {
                Requester::Call { source, id } => FaultOrigin::Control {
                    machine: self.reply_target(source)?,
                    call: *id,
                },
                Requester::Trigger(trigger) => FaultOrigin::Trigger { trigger: *trigger },
                Requester::Input(key) => FaultOrigin::Input {
                    key: super::InputKey::validated(key.clone()),
                },
            },
            target: match scope {
                Scope::Machine { name, boot } => super::FaultTarget::Machine {
                    machine: if *action == FaultAction::Effect(Effect::Spawn) {
                        self.spawn_target(name)?
                    } else {
                        self.named(name)?
                    },
                    boot: *boot,
                },
                Scope::DirectedLinks { from, to } => {
                    let side = |names: &[String]| {
                        let mut ids = names
                            .iter()
                            .map(|n| self.named(n))
                            .collect::<Result<Vec<_>, _>>()?;
                        ids.sort();
                        Ok::<_, String>(ids)
                    };
                    super::FaultTarget::DirectedLinks {
                        from: side(from)?,
                        to: side(to)?,
                    }
                }
                Scope::Connection { connection } => super::FaultTarget::Connection {
                    connection: connection.clone(),
                },
                Scope::Links { left, right } => {
                    let side = |names: &[String]| {
                        let mut ids = names
                            .iter()
                            .map(|name| self.named(name))
                            .collect::<Result<Vec<_>, _>>()?;
                        ids.sort();
                        Ok::<_, String>(ids)
                    };
                    super::FaultTarget::Links {
                        left: side(left)?,
                        right: side(right)?,
                    }
                }
            },
            action: match action {
                FaultAction::Effect(Effect::Spawn) => Public::Spawn,
                FaultAction::Heal => Public::Heal,
                FaultAction::Effect(Effect::Pause) => Public::Pause,
                FaultAction::Effect(Effect::Resume) => Public::Resume,
                FaultAction::Effect(Effect::Crash) => Public::Crash,
                FaultAction::Effect(Effect::Reboot) => Public::Reboot,
                FaultAction::Effect(Effect::Shutdown) => Public::Shutdown,
                FaultAction::Effect(Effect::Execution(config)) => Public::SetExecution(*config),
                FaultAction::Effect(Effect::Drift(rate)) => Public::Drift(*rate),
                FaultAction::Effect(Effect::StepRealtime(nanoseconds)) => {
                    Public::StepRealtime(*nanoseconds)
                }
                FaultAction::Effect(Effect::Process(command)) => Public::Application(*command),
                FaultAction::Effect(Effect::Partition) => Public::Partition,
                FaultAction::Effect(Effect::Shape(policy, faults)) => Public::Shape {
                    policy: *policy,
                    network_faults: *faults,
                },
                FaultAction::Effect(Effect::Select) => Public::DropConnection,
                FaultAction::Effect(Effect::Unshape | Effect::Unselect) => {
                    return Err("network inverse outside a heal".into())
                }
                FaultAction::Effect(Effect::Unpartition) => {
                    return Err("unpartition outside a heal".into())
                }
            },
            handle: handle.clone().map(super::FaultHandle),
            requested_at: Timestamp::from_nanos(requested_at),
        })
    }
    pub(super) fn event(
        &self,
        notice: crate::oracle::service::Notice,
    ) -> Result<(Timestamp, super::EventKind), String> {
        use super::{EventKind, TriggerResult};
        use crate::oracle::{service::Notice, triggers::Status};
        Ok(match notice {
            Notice::Admitted(a) => (
                Timestamp::from_nanos(a.admitted_at),
                EventKind::FaultAdmitted(self.record(
                    a.fault,
                    &a.requester,
                    &a.scope,
                    &a.action,
                    &a.handle,
                    a.requested_at,
                )?),
            ),
            Notice::Settled(s) => (
                Timestamp::from_nanos(s.committed_at),
                EventKind::FaultSettled {
                    record: self.record(
                        s.fault,
                        &s.requester,
                        &s.scope,
                        &s.action,
                        &s.handle,
                        s.requested_at,
                    )?,
                    status: s.status,
                    outcome_at: s.outcome_at.map(Timestamp::from_nanos),
                    detail: s.detail,
                    network: s.network,
                },
            ),
            Notice::TriggerArmed {
                trigger,
                source,
                target,
                at,
                deadline,
            } => (
                Timestamp::from_nanos(at),
                EventKind::TriggerArmed {
                    trigger,
                    source: self.named(&source)?,
                    target: target.map(|target| self.named(&target)).transpose()?,
                    deadline: Timestamp::from_nanos(deadline),
                },
            ),
            Notice::TriggerResolved {
                trigger,
                at,
                status,
                fault,
            } => (
                Timestamp::from_nanos(at),
                EventKind::TriggerResolved {
                    trigger,
                    result: match status {
                        Status::Fired => TriggerResult::Fired,
                        Status::Rejected => TriggerResult::Rejected,
                        Status::Cancelled => TriggerResult::Cancelled,
                        Status::Expired => TriggerResult::Expired,
                        Status::CutOff => TriggerResult::CutOff,
                        Status::Armed | Status::Matched => {
                            return Err("unresolved trigger notice".into())
                        }
                    },
                    fault: fault.map(super::FaultId),
                },
            ),
        })
    }
}

mod persist;
