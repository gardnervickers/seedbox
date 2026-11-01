//! Zero-time application of timeline items due at the current barrier:
//! control events, effect plans, machine inputs, driver events, then
//! timer wakeups.
use super::*;

/// The dispatch check of a due effect plan: `None` when an earlier terminal
/// observation already settled it.
type DispatchCheck = Option<Result<(), crate::oracle::OperationError>>;

/// Mechanical work of one due timeline item, with the control context that
/// admitted it.
struct DueItem {
    /// An `Input` or `Driver` item; control events resolve before this.
    scheduled: Scheduled,
    /// The boot a service reply's call was made from.
    reply_boot: Option<u64>,
    /// The fault whose effect this item applies.
    accepted_fault: Option<u32>,
    /// A fault settled Applied once this non-guest effect takes effect.
    service_effect: Option<u32>,
    /// The origin of a service-issued guest command.
    service_origin: Option<crate::oracle::observation::Origin>,
    /// A healed device fault whose counts settle its heal.
    healed: Option<(MachineId, u64)>,
}

impl DueItem {
    fn new(scheduled: Scheduled) -> Self {
        Self {
            scheduled,
            reply_boot: None,
            accepted_fault: None,
            service_effect: None,
            service_origin: None,
            healed: None,
        }
    }

    /// A simulation-control reply to the call `id` made from `boot`.
    fn reply(
        target: MachineId,
        boot: u64,
        id: crate::pv::SimulationControlId,
        payload: Vec<u8>,
        at: Timestamp,
    ) -> Self {
        Self {
            reply_boot: Some(boot),
            ..Self::new(Scheduled::Input {
                id: target,
                input: Input {
                    at,
                    payload: Request::SimulationReply { id, payload },
                },
            })
        }
    }
}

/// What the mechanical work of a due item changed, published only after the
/// item takes effect without error.
struct Applied {
    /// A lifecycle or setting transition, confirmed through `Control::admission`.
    transition: Option<(MachineId, crate::oracle::observation::Transition)>,
    effective: Option<EventKind>,
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    pub(super) fn apply_due(
        &mut self,
        host_now: Instant,
    ) -> Result<Option<FaultRecord>, Error<M::Error>> {
        self.apply_due_mode(host_now, false)
    }

    pub(super) fn apply_due_mode(
        &mut self,
        host_now: Instant,
        hold_paused: bool,
    ) -> Result<Option<FaultRecord>, Error<M::Error>> {
        let result = self.apply_due_inner(host_now, hold_paused);
        // Facts of a final uncommitted item belong to the current barrier.
        let flushed = self.flush_events();
        let result = result?;
        flushed.map(|()| result)
    }

    fn apply_due_inner(
        &mut self,
        host_now: Instant,
        hold_paused: bool,
    ) -> Result<Option<FaultRecord>, Error<M::Error>> {
        if self.execution.finished.is_some() {
            return Ok(None);
        }
        let now = self.now();
        // Keep the front event until its effect is admitted, including across
        // a pre-fault stop. Driver controls and requests share one FIFO timeline.
        // Frames due now follow every timeline item due now, then timers.
        loop {
            while let Some(scheduled) = self.due_front(now) {
                // Facts of the previous item that committed no barrier belong to
                // the current one, before this item can commit another.
                self.flush_events()?;
                let checked = match self.check_due_effect(&scheduled, host_now, hold_paused)? {
                    ControlFlow::Break(record) => return Ok(Some(record)),
                    ControlFlow::Continue(checked) => checked,
                };
                self.pop_due(now);
                let item = match scheduled {
                    Scheduled::Control(event) => match self.apply_control(event, checked, now)? {
                        Some(item) => item,
                        None => continue,
                    },
                    Scheduled::PropertyReply {
                        machine,
                        boot,
                        session,
                        payload,
                    } => {
                        let entry = &self.execution.machines[machine.0];
                        if entry.lifecycle.boot() != boot
                            || matches!(entry.stopped, StopReason::Exited { .. })
                        {
                            continue;
                        }
                        DueItem::new(Scheduled::Input {
                            id: machine,
                            input: Input {
                                at: now,
                                payload: Request::PropertyReply { session, payload },
                            },
                        })
                    }
                    scheduled => DueItem::new(scheduled),
                };
                self.apply_due_item(item, now)?;
                if self.execution.finished.is_some() {
                    return Ok(None);
                }
            }
            if !self.deliver_frames(now)? {
                break;
            }
            if self.execution.finished.is_some() {
                return Ok(None);
            }
        }
        self.wake_due_timers(now)?;
        Ok(None)
    }

    /// The front timeline item due at `now`, left in place.
    fn due_front(&self, now: Timestamp) -> Option<Scheduled> {
        self.execution
            .events
            .get(&now)
            .and_then(|events| events.front())
            .cloned()
    }

    /// Remove the front timeline item due at `now`.
    fn pop_due(&mut self, now: Timestamp) {
        let events = self.execution.events.get_mut(&now).expect("due events");
        events.pop_front();
        if events.is_empty() {
            self.execution.events.remove(&now);
        }
    }

    /// An effect plan is effective when its dispatch check passes. That
    /// check runs before the plan leaves the FIFO front, so a hold keeps
    /// this exact plan, and every later same-time item, pending. Any other
    /// item continues unchecked (`None`).
    fn check_due_effect(
        &mut self,
        scheduled: &Scheduled,
        host_now: Instant,
        hold_paused: bool,
    ) -> Result<ControlFlow<FaultRecord, Option<DispatchCheck>>, Error<M::Error>> {
        let Scheduled::Control(control::Event::Plan(crate::oracle::service::Plan::Effect {
            fault,
            ..
        })) = scheduled
        else {
            return Ok(ControlFlow::Continue(None));
        };
        let fault = *fault;
        let check = self
            .execution
            .control
            .invoke(&self.execution.machines, |service, view| {
                service.dispatch_check(view, fault)
            })
            .map_err(Error::Control)?;
        if matches!(check, Some(Ok(()))) {
            let was_held = self.playback.held.take().is_some();
            if self.playback.before_next && !was_held {
                let record = self
                    .execution
                    .control
                    .pending_record(fault)
                    .map_err(Error::Control)?;
                self.playback
                    .hold(record.clone(), self.position(), host_now);
                return Ok(ControlFlow::Break(record));
            }
            if hold_paused && !was_held && self.playback.holds_unarmed_effect() {
                let record = self
                    .execution
                    .control
                    .pending_record(fault)
                    .map_err(Error::Control)?;
                self.playback.held = Some(record.clone());
                return Ok(ControlFlow::Break(record));
            }
        }
        Ok(ControlFlow::Continue(Some(check)))
    }

    /// Apply a due control event. Returns the machine work it schedules, or
    /// `None` when the event completed, or was discarded, here.
    fn apply_control(
        &mut self,
        event: control::Event,
        checked: Option<DispatchCheck>,
        now: Timestamp,
    ) -> Result<Option<DueItem>, Error<M::Error>> {
        use crate::oracle::service::Plan;
        match event {
            control::Event::Reply {
                target,
                boot,
                id,
                payload,
            } => {
                let entry = &self.execution.machines[target.0];
                if entry.lifecycle.boot() != boot
                    || matches!(entry.stopped, StopReason::Exited { .. })
                {
                    return Ok(None);
                }
                Ok(Some(DueItem::reply(target, boot, id, payload, now)))
            }
            control::Event::Call {
                source,
                boot,
                id,
                observed,
                payload,
            } => {
                self.admit_call(source, boot, id, observed, &payload, now)?;
                Ok(None)
            }
            control::Event::Input {
                key,
                action,
                handle,
            } => {
                self.admit_input(key, action, handle, now)?;
                self.commit_boundary()?;
                Ok(None)
            }
            control::Event::TriggerBoundary => {
                self.execution
                    .events
                    .entry(now)
                    .or_default()
                    .push_back(Scheduled::Control(control::Event::TriggerFinalize));
                Ok(None)
            }
            control::Event::TriggerFinalize => {
                // An expired block_request trigger releases its machine hold.
                let releases = self
                    .execution
                    .control
                    .service
                    .expire_triggers(now.as_nanos());
                self.control_plans(releases)?;
                self.sync_trigger_marker();
                Ok(None)
            }
            control::Event::Plan(Plan::Wake { token, .. }) => {
                let plan = self
                    .execution
                    .control
                    .service
                    .wake(token, now.as_nanos())
                    .map_err(|e| Error::Control(e.message))?;
                self.control_plans(vec![plan])?;
                Ok(None)
            }
            control::Event::Plan(Plan::Deadline { fault, .. }) => {
                // A Failed settlement publishes through service notices.
                let plans = self
                    .execution
                    .control
                    .service
                    .deadline(fault, now.as_nanos())
                    .map_err(|failure| Error::Control(failure.message))?;
                self.control_plans(plans)?;
                Ok(None)
            }
            control::Event::Plan(Plan::Reply {
                source,
                id,
                payload,
                ..
            }) => {
                let target = self
                    .execution
                    .control
                    .reply_target(&source)
                    .map_err(Error::Control)?;
                let boot = self
                    .execution
                    .control
                    .take_call(target, id)
                    .map_err(Error::Control)?;
                Ok(Some(DueItem::reply(target, boot, id, payload, now)))
            }
            control::Event::Plan(Plan::Effect {
                fault,
                scope,
                effect,
                command,
                handle,
                ..
            }) => match checked.expect("checked effect plan") {
                None => Ok(None),
                Some(Err(reason)) => {
                    self.settle_failed_dispatch(fault, reason, now)?;
                    Ok(None)
                }
                Some(Ok(())) => self.apply_effect(fault, scope, effect, command, handle, now),
            },
            control::Event::Plan(Plan::Settled(_)) => {
                // Receipts transfer in invoke_plans and are consumed
                // by control_plans; they are never executable work.
                Err(Error::Control("settlement entered execution queue".into()))
            }
            control::Event::Plan(Plan::Hold { .. } | Plan::Release { .. }) => {
                // control_plans sends holds to their machine at once.
                Err(Error::Control(
                    "machine hold entered execution queue".into(),
                ))
            }
        }
    }

    /// Admit a guest simulation-control call through the control policy and
    /// commit its plans. With a controller configured, a call from a retired
    /// or exited boot is discarded.
    fn admit_call(
        &mut self,
        source: MachineId,
        boot: u64,
        id: crate::pv::SimulationControlId,
        observed: u64,
        payload: &[u8],
        now: Timestamp,
    ) -> Result<(), Error<M::Error>> {
        let configured = self.execution.control.controller.is_some();
        if configured
            && (self.execution.machines[source.0].lifecycle.boot() != boot
                || matches!(
                    self.execution.machines[source.0].stopped,
                    StopReason::Exited { .. }
                ))
        {
            return Ok(());
        }
        let excluded = self
            .execution
            .control
            .controller
            .and_then(|setup| setup.controller);
        let engine = self.execution.engine;
        if !configured {
            self.execution.control.record_call(source, id, boot);
        }
        let plans = self
            .execution
            .control
            .invoke_with_network(
                &self.execution.machines,
                Some(&self.execution.network),
                |service, view| {
                    view.guest_controller = excluded;
                    view.engine = engine;
                    if configured {
                        service.admit_for_boot(
                            view,
                            (control::Control::source(source), boot),
                            id,
                            observed,
                            now.as_nanos(),
                            payload,
                        )
                    } else {
                        service.admit(
                            view,
                            control::Control::source(source),
                            id,
                            observed,
                            now.as_nanos(),
                            payload,
                        )
                    }
                },
            )
            .map_err(Error::Control)?;
        self.control_plans(plans)?;
        self.commit_boundary()
    }

    /// Settle an effect plan whose dispatch check failed: one blocked by an
    /// active handle collision fails without effect; any other is rejected.
    fn settle_failed_dispatch(
        &mut self,
        fault: u32,
        reason: crate::oracle::OperationError,
        now: Timestamp,
    ) -> Result<(), Error<M::Error>> {
        let collision = self.execution.control.service.handle_collision(fault);
        let plans = self
            .execution
            .control
            .invoke_plans(&self.execution.machines, |service, _| {
                if collision {
                    service.failed_without_effect(fault, now.as_nanos(), reason.message)
                } else {
                    service.rejected(fault, now.as_nanos(), now.as_nanos(), reason)
                }
            })
            .map_err(Error::Control)?;
        self.control_plans(plans)
    }

    /// Apply an effect plan whose dispatch check passed. Link and spawn
    /// effects complete here; a machine effect returns its machine work.
    fn apply_effect(
        &mut self,
        fault: u32,
        scope: crate::oracle::service::Scope,
        effect: crate::oracle::service::Effect,
        command: Option<NonZeroU32>,
        handle: Option<String>,
        now: Timestamp,
    ) -> Result<Option<DueItem>, Error<M::Error>> {
        use crate::oracle::service::{Effect, Scope};
        let (target, boot) = match &scope {
            Scope::Machine { name, boot } => (name.clone(), *boot),
            Scope::Connection { connection } => {
                self.apply_connection(fault, connection, &effect, handle)?;
                self.commit_boundary()?;
                return Ok(None);
            }
            Scope::DirectedLinks {
                from: left,
                to: right,
            }
            | Scope::Links { left, right } => {
                self.apply_links(fault, left, right, &effect, handle)?;
                self.commit_boundary()?;
                return Ok(None);
            }
        };
        if effect == Effect::Spawn {
            self.apply_spawn(&target, fault, now)?;
            return Ok(None);
        }
        let id = self
            .execution
            .control
            .reply_target(&target)
            .map_err(Error::Control)?;
        debug_assert_eq!(self.execution.machines[id.0].lifecycle.boot(), boot);
        let service_effect = (!effect.guest_command()).then_some(fault);
        let healed = match effect {
            Effect::Unfault { record } => Some((id, record)),
            _ => None,
        };
        let seed = self.execution.control.fault_seed(id, fault);
        let (scheduled, service_origin) =
            Self::machine_effect(id, fault, seed, effect, command, now)?;
        Ok(Some(DueItem {
            scheduled,
            reply_boot: None,
            accepted_fault: Some(fault),
            service_effect,
            service_origin,
            healed,
        }))
    }

    /// Construct a spawned machine and settle its effect as applied. A failed
    /// construction leaves execution state uncertain.
    fn apply_spawn(
        &mut self,
        target: &str,
        fault: u32,
        now: Timestamp,
    ) -> Result<(), Error<M::Error>> {
        let created = self.spawn(target, fault);
        if let Err(error) = created {
            self.execution.control.service.execution_failed(
                fault,
                None,
                now.as_nanos(),
                "spawn construction failed".into(),
            );
            self.uncertain_failure();
            return Err(error);
        }
        let plans = self
            .execution
            .control
            .invoke_plans(&self.execution.machines, |service, _| {
                service.applied(fault, now.as_nanos(), now.as_nanos())
            })
            .map_err(Error::Control)?;
        self.control_plans(plans)?;
        self.commit_boundary()
    }

    /// The machine work of a checked effect on machine `id`, with the origin
    /// of a service-issued guest command. `seed` keys a device fault's draws.
    fn machine_effect(
        id: MachineId,
        fault: u32,
        seed: u64,
        effect: crate::oracle::service::Effect,
        command: Option<NonZeroU32>,
        now: Timestamp,
    ) -> Result<(Scheduled, Option<crate::oracle::observation::Origin>), Error<M::Error>> {
        use crate::oracle::service::Effect;
        let input = |payload| Scheduled::Input {
            id,
            input: Input { at: now, payload },
        };
        let service = Some(crate::oracle::observation::Origin::Service { fault });
        Ok(match effect {
            Effect::Spawn => unreachable!("spawn handled separately"),
            Effect::Partition
            | Effect::Unpartition
            | Effect::Shape(..)
            | Effect::Unshape
            | Effect::Select
            | Effect::Unselect => {
                return Err(Error::Control("link effect bound to a machine".into()));
            }
            Effect::Pause => (Scheduled::Driver(DriverEvent::Pause(id)), None),
            Effect::Resume => (Scheduled::Driver(DriverEvent::Resume(id)), None),
            Effect::Execution(config) => (
                Scheduled::Driver(DriverEvent::SetExecution {
                    machine: id,
                    config,
                }),
                None,
            ),
            Effect::Drift(rate) => (
                Scheduled::Driver(DriverEvent::SetClockRate { machine: id, rate }),
                None,
            ),
            Effect::StepRealtime(nanoseconds) => (
                input(Request::StepRealtime {
                    id: command.ok_or_else(|| Error::Control("clock plan lacks command".into()))?,
                    nanoseconds: std::num::NonZeroI64::new(nanoseconds)
                        .ok_or_else(|| Error::Control("zero clock step plan".into()))?,
                }),
                service,
            ),
            Effect::Crash => (input(Request::Crash), None),
            Effect::Reboot => (input(Request::Reboot), None),
            Effect::Fault { record, fault } => (
                input(Request::InstallFault {
                    id: record,
                    seed,
                    fault,
                }),
                None,
            ),
            Effect::Unfault { record } => (input(Request::HealFault { id: record }), None),
            Effect::Shutdown => (input(Request::Shutdown), None),
            Effect::Process(action) => (
                input(Request::Application {
                    id: command
                        .ok_or_else(|| Error::Control("process plan lacks command".into()))?,
                    command: action,
                }),
                service,
            ),
        })
    }

    /// Apply the mechanical work of a due item. Only once it takes effect
    /// without error are its fact staged, its transition confirmed and a
    /// service effect settled, before the barrier commits.
    fn apply_due_item(&mut self, item: DueItem, now: Timestamp) -> Result<(), Error<M::Error>> {
        let DueItem {
            scheduled,
            reply_boot,
            accepted_fault,
            service_effect,
            service_origin,
            healed,
        } = item;
        let cause = accepted_fault.map_or(Cause::Host, |fault| Cause::Fault(FaultId(fault)));
        let mechanical = match scheduled {
            Scheduled::Control(_) | Scheduled::PropertyReply { .. } => unreachable!(),
            Scheduled::Input { id, input } => {
                self.apply_input(id, input, reply_boot, service_origin, cause, now)
            }
            Scheduled::Driver(event) => self.apply_driver_event(event, cause, now),
        };
        let applied = match mechanical {
            Ok(applied) => applied,
            Err(error) => {
                if let Some(fault) = accepted_fault {
                    // The Failed settlement publishes through service notices.
                    let _ = self.execution.control.service.execution_failed(
                        fault,
                        None,
                        now.as_nanos(),
                        "machine effect failed; execution state is uncertain".into(),
                    );
                }
                self.uncertain_failure();
                return Err(error);
            }
        };
        if let Some(kind) = applied.effective {
            self.stage(now, kind);
        }
        if let Some((id, transition)) = applied.transition {
            let plans = self
                .execution
                .control
                .admission(&mut self.execution.machines, id, now, transition)
                .map_err(Error::Control)?;
            self.control_plans(plans)?;
        }
        if let Some(fault) = service_effect {
            // A healed device fault settles with what it did over every boot.
            let counts = match healed {
                Some((id, record)) => self.execution.machines[id.0]
                    .machine
                    .as_mut()
                    .expect("stopped machine")
                    .take_fault_counts(record)
                    .map_err(|source| Error::Machine { id, source })?,
                None => None,
            };
            let plans = self
                .execution
                .control
                .invoke_plans(&self.execution.machines, |service, _| {
                    service.applied_counts(fault, now.as_nanos(), counts)
                })
                .map_err(Error::Control)?;
            self.control_plans(plans)?;
        }
        self.commit_boundary()
    }

    /// Queue a due machine request and give it a zero-budget admission run.
    fn apply_input(
        &mut self,
        id: MachineId,
        input: Input,
        reply_boot: Option<u64>,
        service_origin: Option<crate::oracle::observation::Origin>,
        cause: Cause,
        now: Timestamp,
    ) -> Result<Applied, Error<M::Error>> {
        if let Some((command, action)) = control::command(&input.payload) {
            // Public scheduling creates guest commands only from
            // admitted faults. Only private queue tests inject raw
            // (external) application requests.
            let origin = service_origin.unwrap_or(crate::oracle::observation::Origin::External);
            if !matches!(
                self.execution.machines[id.0].stopped,
                StopReason::Exited { .. }
            ) {
                self.execution
                    .control
                    .offered(id, command, action, origin)
                    .map_err(Error::Control)?;
            }
        }
        use crate::oracle::observation::Transition;
        let boot = self.execution.machines[id.0].lifecycle.boot();
        let new_boot = match input.payload {
            Request::Reboot => Some(boot.checked_add(1).ok_or(Error::BootOverflow { id })?),
            _ => None,
        };
        let transition = match input.payload {
            Request::Crash => Some(Transition::Crash),
            Request::Reboot => new_boot.map(|new_boot| Transition::Reboot { new_boot }),
            _ => None,
        };
        // The pause gate is cleared when this transition commits.
        let transition = transition.map(|transition| (id, transition));
        let entry = &mut self.execution.machines[id.0];
        match &input.payload {
            Request::Reboot | Request::Crash => {
                if id == self.execution.oracle {
                    self.execution.oracle_stops.clear();
                }
                entry.inbox.clear();
            }
            Request::Application { .. }
            | Request::StepRealtime { .. }
            | Request::SimulationReply { .. }
            | Request::PropertyReply { .. }
            | Request::ReceiveFrame { .. }
                if matches!(entry.stopped, StopReason::Exited { .. }) =>
            {
                return Ok(Applied {
                    transition,
                    effective: None,
                });
            }
            Request::Application { .. }
            | Request::StepRealtime { .. }
            | Request::SimulationReply { .. }
            | Request::PropertyReply { .. }
            | Request::ReceiveFrame { .. }
            | Request::Shutdown
            // Device faults and holds outlive the boot: an exited machine
            // keeps them for its next one.
            | Request::InstallFault { .. }
            | Request::HealFault { .. }
            | Request::HoldBlockRequest { .. }
            | Request::ReleaseBlockRequest { .. } => {}
            // Request is non-exhaustive across the common crate boundary.
            _ => {
                return Err(Error::Control("unsupported machine request".into()));
            }
        }
        let exited = matches!(entry.stopped, StopReason::Exited { .. });
        let change = |change| EventKind::Transition {
            machine: id,
            boot,
            change,
            cause,
        };
        let effective = match &input.payload {
            Request::Crash => Some(change(event::Transition::Crashed)),
            Request::Reboot => {
                new_boot.map(|new_boot| change(event::Transition::Rebooted { new_boot }))
            }
            Request::Shutdown if !exited => Some(change(event::Transition::ShutdownRequested)),
            // A call from an earlier boot belongs to a retired
            // session, which the machine silently discards.
            Request::SimulationReply { id: call, .. }
                if reply_boot.is_none_or(|called| called == boot) =>
            {
                Some(EventKind::ReplyAdmitted {
                    machine: id,
                    boot,
                    id: *call,
                })
            }
            _ => None,
        };
        entry.inbox.push_back(input);
        // A Reboot admission run executes the prospective boot.
        self.run_batch_as(&[id], now, new_boot)?;
        Ok(Applied {
            transition,
            effective,
        })
    }

    /// Apply a due pause, resume, execution setting or clock rate change.
    fn apply_driver_event(
        &mut self,
        event: DriverEvent,
        cause: Cause,
        now: Timestamp,
    ) -> Result<Applied, Error<M::Error>> {
        let (DriverEvent::Pause(id)
        | DriverEvent::Resume(id)
        | DriverEvent::SetExecution { machine: id, .. }
        | DriverEvent::SetClockRate { machine: id, .. }) = event;
        use crate::oracle::observation::Transition;
        let entry = &self.execution.machines[id.0];
        let transition = match event {
            DriverEvent::SetExecution { config, .. } => Some(Transition::Execution(config)),
            DriverEvent::SetClockRate { rate, .. } => Some(Transition::ClockRate(rate)),
            DriverEvent::Pause(_)
                if !entry.lifecycle.paused()
                    && !matches!(entry.stopped, StopReason::Exited { .. }) =>
            {
                Some(Transition::Pause)
            }
            DriverEvent::Resume(_) if entry.lifecycle.paused() => Some(Transition::Resume),
            _ => None,
        };
        let applied = Applied {
            transition: transition.map(|transition| (id, transition)),
            effective: transition.map(|transition| EventKind::Transition {
                machine: id,
                boot: entry.lifecycle.boot(),
                change: match transition {
                    Transition::Pause => event::Transition::Paused,
                    Transition::Resume => event::Transition::Resumed,
                    Transition::Execution(config) => event::Transition::ExecutionSet(config),
                    Transition::ClockRate(rate) => event::Transition::ClockRate(rate),
                    Transition::Crash | Transition::Reboot { .. } => unreachable!(),
                },
                cause,
            }),
        };
        // Pause, resume and execution settings change only
        // the lifecycle record, when the transition commits.
        let entry = &mut self.execution.machines[id.0];
        match event {
            DriverEvent::SetExecution { .. } | DriverEvent::Pause(_) => {}
            DriverEvent::SetClockRate { rate, .. } => {
                // Reanchor keeping the mapped time at this
                // barrier, then re-convert a pending timer.
                entry
                    .time
                    .set_drift(now.as_nanos(), i64::from(rate.ppb()))
                    .map_err(|_| Error::ClockOverflow { id })?;
                if let (StopReason::Blocked { reached, .. }, Some(timer)) =
                    (entry.stopped, entry.timer)
                {
                    entry.stopped = StopReason::Blocked {
                        reached,
                        deadline: entry.simulation_time(now, timer),
                    };
                }
            }
            DriverEvent::Resume(_)
                if transition.is_some() && !matches!(entry.stopped, StopReason::Exited { .. }) =>
            {
                self.run_batch(&[id], now)?;
            }
            DriverEvent::Resume(_) => {}
        }
        Ok(applied)
    }

    /// Give every running machine whose timer is due at `now` a zero-budget
    /// run and commit.
    fn wake_due_timers(&mut self, now: Timestamp) -> Result<(), Error<M::Error>> {
        let mut waking = Vec::new();
        for index in 0..self.execution.machines.len() {
            let stopped = self.execution.machines[index].stopped;
            if self.execution.machines[index].lifecycle.paused()
                || matches!(stopped, StopReason::Exited { .. })
            {
                continue;
            }
            if matches!(stopped,
                StopReason::Blocked { deadline: Some(at), .. } if at == now)
            {
                // Let the machine observe the new clock and deliver its timer,
                // without executing beyond this barrier.
                waking.push(MachineId(index));
            }
        }
        self.run_batch(&waking, now)?;
        if !waking.is_empty() {
            self.commit_boundary()?;
        }
        Ok(())
    }
}
