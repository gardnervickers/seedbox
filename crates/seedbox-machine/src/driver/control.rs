//! Concrete private control ownership. Machine identities route replies;
//! optional display names resolve targets, never the reverse.
use super::{MachineId, Timestamp};
use crate::{
    definition::MachineDefinition,
    execution::ExecutionConfig,
    oracle::{
        observation::{Origin, State},
        service::{Adapter, CommandOwner, Facts, Service},
        OperationError, Target,
    },
    pv::ApplicationCommand,
};
use serde_json::Value;
use std::{collections::BTreeMap, num::NonZeroU32};

#[derive(Default)]
struct Commands {
    next: u32,
    future: BTreeMap<NonZeroU32, usize>,
    used: BTreeMap<NonZeroU32, (ApplicationCommand, Origin, bool)>,
}
impl Commands {
    fn reserve(&mut self, fault: u32, action: ApplicationCommand) -> Result<NonZeroU32, String> {
        loop {
            self.next = self
                .next
                .checked_add(1)
                .ok_or("application command IDs exhausted")?;
            let id = NonZeroU32::new(self.next).unwrap();
            if !self.future.contains_key(&id) && !self.used.contains_key(&id) {
                self.used
                    .insert(id, (action, Origin::Service { fault }, false));
                return Ok(id);
            }
        }
    }
    fn reset(&mut self) {
        self.next = 0;
        self.used.clear();
    }
}
struct Observed {
    name: Option<String>,
    state: State,
    commands: Commands,
}
pub(super) struct Control {
    pub(super) service: Service,
    entries: Vec<Observed>,
    pub(super) settlements: Vec<crate::oracle::service::Settlement>,
}
impl Control {
    pub(super) fn new(definitions: &[MachineDefinition], start: Timestamp) -> Self {
        let entries = definitions
            .iter()
            .map(|definition| Observed {
                name: Some(definition.name.clone()),
                state: State::new(
                    definition.name.clone(),
                    0,
                    start.as_nanos(),
                    ExecutionConfig::default(),
                ),
                commands: Commands::default(),
            })
            .collect();
        Self {
            service: Service::default(),
            settlements: vec![],
            entries,
        }
    }
    pub(super) fn add_unnamed(&mut self, at: Timestamp) {
        let key = Self::source(MachineId(self.entries.len()));
        self.entries.push(Observed {
            name: None,
            state: State::new(key, 0, at.as_nanos(), ExecutionConfig::default()),
            commands: Commands::default(),
        });
    }
    pub(super) fn source(id: MachineId) -> String {
        format!("machine:{}", id.0)
    }
    pub(super) fn reply_target(&self, source: &str) -> Result<MachineId, String> {
        let index = source
            .strip_prefix("machine:")
            .and_then(|index| index.parse::<usize>().ok())
            .ok_or("invalid private control source")?;
        if index >= self.entries.len() {
            return Err("unknown private control source".into());
        }
        Ok(MachineId(index))
    }
    pub(super) fn target(&self, name: &str) -> Result<MachineId, OperationError> {
        let mut matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.as_deref() == Some(name));
        let Some((index, _)) = matches.next() else {
            return Err(OperationError::invalid("unknown machine target"));
        };
        if matches.next().is_some() {
            return Err(OperationError::invalid("ambiguous machine target"));
        }
        Ok(MachineId(index))
    }
    pub(super) fn reserve_external(
        &mut self,
        id: MachineId,
        command: NonZeroU32,
    ) -> Result<(), String> {
        let count = self.entries[id.0]
            .commands
            .future
            .entry(command)
            .or_default();
        *count = count
            .checked_add(1)
            .ok_or("future command reservation count overflow")?;
        Ok(())
    }
    pub(super) fn retire_external(
        &mut self,
        id: MachineId,
        command: NonZeroU32,
    ) -> Result<(), String> {
        let commands = &mut self.entries[id.0].commands;
        let count = commands
            .future
            .get_mut(&command)
            .ok_or("missing future command reservation")?;
        *count -= 1;
        if *count == 0 {
            commands.future.remove(&command);
        }
        Ok(())
    }
    pub(super) fn offered(
        &mut self,
        id: MachineId,
        command: NonZeroU32,
        action: ApplicationCommand,
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
        boot: u64,
        command: NonZeroU32,
        at: Timestamp,
        commit: Timestamp,
    ) -> Result<(), String> {
        let entry = &mut self.entries[id.0];
        let (action, origin, accepted) = entry
            .commands
            .used
            .get_mut(&command)
            .ok_or("unreserved application command")?;
        if !*accepted {
            entry
                .state
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
    pub(super) fn current_boot(&self, id: MachineId) -> Result<u64, String> {
        self.entries[id.0]
            .state
            .view()
            .map(|view| view.boot)
            .map_err(|e| format!("observation: {e:?}"))
    }
    pub(super) fn reset_commands(&mut self, id: MachineId) {
        self.entries[id.0].commands.reset();
    }
}
pub(super) struct View<'a> {
    entries: &'a mut [Observed],
}
impl View<'_> {
    fn index(&self, name: &str) -> Result<usize, String> {
        let mut matches = self
            .entries
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
}
impl Adapter for View<'_> {
    fn resolve(&self, target: &Target) -> Result<Facts, OperationError> {
        let Target::Machine(name) = target else {
            return Err(OperationError::unsupported());
        };
        let index = self.index(name).map_err(OperationError::invalid)?;
        self.entries[index]
            .state
            .facts()
            .map_err(|_| OperationError::invalid("unavailable observation"))
    }
    fn command_id(
        &mut self,
        target: &str,
        boot: u64,
        fault: u32,
        action: ApplicationCommand,
    ) -> Result<NonZeroU32, String> {
        let index = self.index(target)?;
        let entry = &mut self.entries[index];
        if entry
            .state
            .view()
            .map_err(|e| format!("observation: {e:?}"))?
            .boot
            != boot
        {
            return Err("command reservation for stale boot".into());
        }
        entry.commands.reserve(fault, action)
    }
    fn command_owner(
        &self,
        target: &str,
        boot: u64,
        id: NonZeroU32,
    ) -> Result<CommandOwner, String> {
        self.entries[self.index(target)?]
            .state
            .command_owner(boot, id)
            .map_err(|e| format!("command correlation: {e:?}"))
    }
    fn inspect(
        &self,
        _: &[String],
        _: &[crate::oracle::triggers::Record],
    ) -> Result<Value, OperationError> {
        Err(OperationError::unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::fixture;
    #[test]
    fn duplicate_display_names_never_alias_sources_and_unnamed_additions_route() {
        let mut control = Control::new(
            &[fixture::definition(0), fixture::definition(0)],
            Timestamp::from_nanos(0),
        );
        assert!(control.target("fixture-0").is_err());
        for index in 0..2 {
            assert_eq!(
                control
                    .reply_target(&Control::source(MachineId(index)))
                    .unwrap(),
                MachineId(index)
            );
        }
        control.add_unnamed(Timestamp::from_nanos(3));
        assert_eq!(
            control
                .reply_target(&Control::source(MachineId(2)))
                .unwrap(),
            MachineId(2)
        );
        assert!(control.target("machine:2").is_err());
    }
    #[test]
    fn boot_allocator_excludes_future_external_ids_and_never_wraps() {
        let mut commands = Commands::default();
        let one = NonZeroU32::new(1).unwrap();
        commands.future.insert(one, 2);
        assert_eq!(
            commands.reserve(7, ApplicationCommand::Stop).unwrap().get(),
            2
        );
        commands.reset();
        assert_eq!(
            commands
                .reserve(8, ApplicationCommand::Start)
                .unwrap()
                .get(),
            2
        );
        commands.next = u32::MAX;
        let before = commands.used.clone();
        assert!(commands.reserve(9, ApplicationCommand::Pause).is_err());
        assert_eq!(commands.used, before);
    }
}

/// Events share the Driver queue, including zero-time calls and expiry markers.
#[derive(Clone)]
pub(super) enum Event {
    Call {
        source: MachineId,
        id: crate::pv::SimulationControlId,
        observed: u64,
        payload: Vec<u8>,
    },
    Plan(crate::oracle::service::Plan),
    TriggerBoundary,
    TriggerFinalize,
}
impl Control {
    pub(super) fn invoke<T>(
        &mut self,
        f: impl FnOnce(&mut Service, &mut View<'_>) -> Result<T, crate::oracle::service::Failure>,
    ) -> Result<T, String> {
        let result = f(
            &mut self.service,
            &mut View {
                entries: &mut self.entries,
            },
        );
        result.map_err(|failure| {
            self.settlements.extend(failure.settlements);
            failure.message
        })
    }
    /// A successful service call transfers immutable settlements now, before
    /// later response/state processing can fail or terminal cutoff can discard
    /// its scheduling plans. Leave the records in the returned plans solely so
    /// the queue owner can retire their already-scheduled deadlines.
    pub(super) fn invoke_plans(
        &mut self,
        f: impl FnOnce(
            &mut Service,
            &mut View<'_>,
        )
            -> Result<Vec<crate::oracle::service::Plan>, crate::oracle::service::Failure>,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        let plans = self.invoke(f)?;
        self.settlements
            .extend(plans.iter().filter_map(|plan| match plan {
                crate::oracle::service::Plan::Settled(settled) => Some(settled.clone()),
                _ => None,
            }));
        Ok(plans)
    }
    pub(super) fn name(&self, id: MachineId) -> Option<String> {
        let name = self.entries[id.0].name.as_ref()?;
        self.target(name)
            .ok()
            .filter(|target| *target == id)
            .map(|_| name.clone())
    }
    pub(super) fn uncertain_failure(&mut self) {
        for entry in &mut self.entries {
            entry.state.uncertain_failure();
        }
    }
    pub(super) fn admission(
        &mut self,
        id: MachineId,
        at: Timestamp,
        transition: crate::oracle::observation::Transition,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        use crate::oracle::observation::{Admission, Transition};
        let boot = self.current_boot(id)?;
        let mut plans = vec![];
        if matches!(transition, Transition::Crash | Transition::Reboot { .. }) {
            if let Some(name) = self.name(id) {
                plans = self.invoke_plans(|service, _| {
                    service.ended(&name, boot, at.as_nanos(), at.as_nanos())
                })?;
                self.service.source_reset(&name).map_err(|e| e.message)?;
            }
        }
        self.entries[id.0]
            .state
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
    pub(super) fn observe(
        &mut self,
        output: &super::MachineResponse,
        commit: Timestamp,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        use crate::pv::Response;
        let mut plans = vec![];
        let name = self.name(output.machine);
        if let Some(name) = &name {
            match &output.payload {
                Response::CommandAcknowledged { id, result } => {
                    plans.extend(self.invoke_plans(|service, view| {
                        service.acknowledged(
                            view,
                            name,
                            output.boot,
                            *id,
                            *result,
                            output.at.as_nanos(),
                            commit.as_nanos(),
                        )
                    })?)
                }
                Response::ApplicationOutcome { .. } => {
                    plans.extend(self.invoke_plans(|service, _| {
                        service.application_ended(
                            name,
                            output.boot,
                            output.at.as_nanos(),
                            commit.as_nanos(),
                        )
                    })?)
                }
                Response::ApplicationOutput { bytes, .. } | Response::BootConsole { bytes } => self
                    .service
                    .committed_bytes(name, output.at.as_nanos(), bytes, commit.as_nanos())
                    .map_err(|e| e.message)?,
                _ => {}
            }
        }
        self.entries[output.machine.0]
            .state
            .response(
                output.boot,
                output.at.as_nanos(),
                commit.as_nanos(),
                &output.payload,
            )
            .map_err(|e| format!("control observation response: {e:?}"))?;
        if matches!(output.payload, Response::Ready) {
            if let Some(name) = name {
                plans.extend(self.invoke_plans(|service, view| {
                    service.ready(
                        view,
                        &name,
                        output.boot,
                        output.at.as_nanos(),
                        commit.as_nanos(),
                    )
                })?);
            }
        }
        Ok(plans)
    }
    pub(super) fn terminal(
        &mut self,
        id: MachineId,
        stopped: crate::machine::StopReason,
        commit: Timestamp,
    ) -> Result<Vec<crate::oracle::service::Plan>, String> {
        let crate::machine::StopReason::Exited { reached, reason } = stopped else {
            return Ok(vec![]);
        };
        let boot = self.current_boot(id)?;
        let change = self.entries[id.0]
            .state
            .terminal(boot, reached.as_nanos(), commit.as_nanos(), reason)
            .map_err(|e| format!("control terminal: {e:?}"))?;
        if change != crate::oracle::observation::Change::Changed {
            return Ok(vec![]);
        }
        if let Some(name) = self.name(id) {
            return self.invoke_plans(|service, _| {
                service.ended(&name, boot, reached.as_nanos(), commit.as_nanos())
            });
        }
        Ok(vec![])
    }
}
