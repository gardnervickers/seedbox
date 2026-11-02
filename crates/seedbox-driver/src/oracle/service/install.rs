//! Fault installation and dispatch: admission checks, pending records,
//! effect plans and their revalidation at FIFO admission.
use super::*;

impl Service {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn install(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        requester: Requester,
        call: Option<Call>,
        target: Subject,
        effect: Effect,
        handle: Option<ExecutionPlan>,
        healing: bool,
        now: u64,
        deadline: u64,
    ) -> Result<(u32, Vec<Plan>), PreparationError> {
        let (scope, facts) = match target {
            Subject::Network(scope) => {
                return self.install_links(
                    adapter, mode, requester, call, scope, effect, handle, healing, now, deadline,
                )
            }
            Subject::Machine(facts) => (
                Scope::Machine {
                    name: facts.name.clone(),
                    boot: facts.boot,
                },
                facts,
            ),
            // Links bind to no boot and have no machine state to check.
            Subject::Links { left, right } => {
                return self.install_links(
                    adapter,
                    mode,
                    requester,
                    call,
                    Scope::Links { left, right },
                    effect,
                    handle,
                    healing,
                    now,
                    deadline,
                );
            }
        };
        Self::check_machine_effect(mode, &requester, &self.requested(&facts), &effect)?;
        let fault = self.token()?;
        let pending = Pending {
            network: None,
            call,
            requester,
            scope,
            effect: effect.clone(),
            command: None,
            dispatched: None,
            deadline,
            admitted: now,
            result: Self::admission_result(&effect, handle.as_ref(), healing, &facts.name),
            note: Self::admission_note(&effect, handle.as_ref()),
            handle,
            healing,
        };
        let unavailable =
            pending.effect.guest_command() && (facts.vm == Vm::Down || facts.physically_ended);
        self.admitted(fault, &pending, now);
        // Healing a step whose kernel ended changes nothing in any state. A
        // zero step (a seeded draw or its heal) needs no guest command, but
        // only after the same admission and availability checks as any step.
        let lost = pending.handle.as_ref().is_some_and(|h| h.lost);
        let immediate = lost || (pending.effect == Effect::StepRealtime(0) && !unavailable);
        self.pending.insert(fault, pending);
        if immediate {
            return Ok((fault, self.finish(fault, now, now, Ok(()))?));
        }
        let mut plans = vec![];
        // A heal queued behind its unacknowledged step is sent once it lands.
        let queued = (self.pending[&fault].handle.as_ref())
            .is_some_and(|h| h.healing() && self.handles.awaiting_start(h.record_id));
        if !unavailable && !queued && (!self.pending[&fault].effect.guest_command() || facts.ready)
        {
            match Self::dispatch(adapter, fault, self.pending.get_mut(&fault).unwrap(), now) {
                Ok(plan) => plans.push(plan),
                Err(e) => return Err(self.fatal(Some(fault), Some(now), now, e.message).into()),
            }
        }
        if self.pending[&fault].effect.guest_command() {
            plans.push(Plan::Deadline {
                fault,
                at: deadline,
            });
        }
        if unavailable {
            plans = self.finish(
                fault,
                now,
                now,
                Err(error(
                    ErrorCode::TargetUnavailable,
                    "target is physically unavailable",
                )),
            )?;
        }
        Ok((fault, plans))
    }
    /// `facts` once the process commands already admitted for its boot land.
    /// The supervisor carries them out in admission order, so a restart
    /// requested while its stop is still being reaped follows that stop
    /// instead of being refused.
    fn requested(&self, facts: &Facts) -> Facts {
        let mut facts = facts.clone();
        for p in self.pending.values() {
            if let (Effect::Process(command), true) =
                (&p.effect, p.scope.is_machine(&facts.name, facts.boot))
            {
                facts.process = match command {
                    ApplicationCommand::Start | ApplicationCommand::Resume => Process::Running,
                    ApplicationCommand::Stop => Process::Stopped,
                    ApplicationCommand::Pause => Process::Frozen,
                };
            }
        }
        facts
    }
    /// Machine-effect admission checks, in order: the control-only
    /// availability preflight, target state policy, then requester mode.
    fn check_machine_effect(
        mode: AdmissionMode,
        requester: &Requester,
        facts: &Facts,
        effect: &Effect,
    ) -> Result<(), PreparationError> {
        // The control API's physical-availability preflight precedes Faults'
        // policy validation (world_windowed::admit_control/reject_unavailable).
        // Trigger admission bypasses this control-only wrapper.
        if matches!(mode, AdmissionMode::Control)
            && facts.physically_ended
            && facts.vm != Vm::Down
            && matches!(
                effect,
                Effect::Pause | Effect::Resume | Effect::Shutdown | Effect::Process(_)
            )
        {
            return Err(error(ErrorCode::TargetUnavailable, "target VM has exited").into());
        }
        let allowed = match effect {
            Effect::Process(command) => {
                facts.vm == Vm::Down
                    || (facts.vm != Vm::Paused
                        && match command {
                            ApplicationCommand::Start => facts.process == Process::Stopped,
                            ApplicationCommand::Stop => facts.process != Process::Stopped,
                            ApplicationCommand::Pause => facts.process == Process::Running,
                            ApplicationCommand::Resume => facts.process == Process::Frozen,
                        })
            }
            Effect::Pause => facts.vm == Vm::Up,
            Effect::Resume => facts.vm == Vm::Paused,
            Effect::Crash => facts.vm != Vm::Down,
            Effect::Reboot => facts.vm == Vm::Down,
            Effect::Shutdown => facts.vm != Vm::Down,
            // Clock effects were checked before drawing their values.
            // Device faults apply in every state, like a throttle: an exited
            // or crashed machine keeps them for its next boot.
            Effect::Execution(_)
            | Effect::Spawn
            | Effect::Drift(_)
            | Effect::StepRealtime(_)
            | Effect::Fault { .. }
            | Effect::Unfault { .. } => true,
            Effect::Partition
            | Effect::Unpartition
            | Effect::Shape(..)
            | Effect::Unshape
            | Effect::Select
            | Effect::Unselect => {
                return Err(Failure::from("link effect bound to a machine").into())
            }
        };
        if !allowed {
            return Err(error(ErrorCode::InvalidState, "target state rejects operation").into());
        }
        if matches!(mode, AdmissionMode::Trigger) != matches!(requester, Requester::Trigger(_)) {
            return Err(Failure::from("admission mode does not match its requester").into());
        }
        Ok(())
    }
    /// The success result an admitted machine effect replies with.
    fn admission_result(
        effect: &Effect,
        handle: Option<&ExecutionPlan>,
        healing: bool,
        machine: &str,
    ) -> Value {
        match handle {
            _ if *effect == Effect::Spawn => json!({"machine":machine}),
            Some(h) if !healing => match effect {
                Effect::StepRealtime(offset) => {
                    json!({"handle":h.handle.to_string(),"offset_ns":offset})
                }
                Effect::Drift(rate) => {
                    json!({"handle":h.handle.to_string(),"rate_ppb":rate.ppb()})
                }
                _ => json!({"handle":h.handle.to_string()}),
            },
            _ => json!({}),
        }
    }
    /// The settlement note of an effect applied without a guest command.
    fn admission_note(effect: &Effect, handle: Option<&ExecutionPlan>) -> Option<&'static str> {
        // These static notes form a closed wire list in persistence.rs.
        // Update that list when adding or changing a note here.
        match handle {
            Some(h) if h.lost => Some("realtime step ended with its machine incarnation"),
            _ if *effect == Effect::StepRealtime(0) => Some("zero realtime step"),
            _ => None,
        }
    }
    fn admitted(&mut self, fault: u32, pending: &Pending, now: u64) {
        self.notices.push(Notice::Admitted(Admitted {
            record_id: pending.handle.as_ref().map(|h| h.record_id),
            fault,
            requester: pending.requester.clone(),
            scope: pending.scope.clone(),
            action: Self::action(pending),
            handle: Self::handle(pending),
            requested_at: Self::requested_at(pending),
            admitted_at: now,
        }));
    }
    /// Admit a partition or its heal. Links have no availability or state
    /// check beyond validated membership: the effect is dispatched at once and
    /// applied at its FIFO turn, whatever the members' lifecycle.
    #[allow(clippy::too_many_arguments)]
    fn install_links(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        requester: Requester,
        call: Option<Call>,
        scope: Scope,
        effect: Effect,
        handle: Option<ExecutionPlan>,
        healing: bool,
        now: u64,
        deadline: u64,
    ) -> Result<(u32, Vec<Plan>), PreparationError> {
        if !matches!(
            effect,
            Effect::Partition
                | Effect::Unpartition
                | Effect::Shape(..)
                | Effect::Unshape
                | Effect::Select
                | Effect::Unselect
        ) {
            return Err(Failure::from("machine effect bound to links").into());
        }
        if matches!(mode, AdmissionMode::Trigger) != matches!(requester, Requester::Trigger(_)) {
            return Err(Failure::from("admission mode does not match its requester").into());
        }
        let network = if effect == Effect::Unselect {
            let plan = handle
                .as_ref()
                .ok_or_else(|| Failure::from("selection heal lacks handle"))?;
            match adapter.selection_counts(&plan.handle.to_string()) {
                Ok(Some(counts)) => Some(counts),
                result => {
                    self.handles
                        .rejected(plan)
                        .map_err(|_| Failure::from("selection heal rollback"))?;
                    return Err(result
                        .err()
                        .unwrap_or_else(|| {
                            error(ErrorCode::InvalidState, "selection handle unavailable")
                        })
                        .into());
                }
            }
        } else {
            None
        };
        let result = if let Some(counts) = &network {
            serde_json::to_value(counts)
                .map_err(|e| Failure::from(format!("network result: {e}")))?
        } else {
            match &handle {
                Some(h) if !healing => json!({"handle":h.handle.to_string()}),
                _ => json!({}),
            }
        };
        let fault = self.token()?;
        let pending = Pending {
            network,
            call,
            requester,
            scope,
            effect,
            command: None,
            dispatched: None,
            deadline,
            admitted: now,
            result,
            note: None,
            handle,
            healing,
        };
        self.admitted(fault, &pending, now);
        self.pending.insert(fault, pending);
        match Self::dispatch(adapter, fault, self.pending.get_mut(&fault).unwrap(), now) {
            Ok(plan) => Ok((fault, vec![plan])),
            Err(e) => Err(self.fatal(Some(fault), Some(now), now, e.message).into()),
        }
    }
    fn dispatch(
        adapter: &mut impl Adapter,
        fault: u32,
        p: &mut Pending,
        at: u64,
    ) -> Result<Plan, Failure> {
        if let Some(action) = p.effect.command_kind() {
            let (target, boot) = p.scope.machine().ok_or("guest command without a machine")?;
            p.command = Some(adapter.command_id(target, boot, fault, action)?);
            p.dispatched = Some(at);
        }
        Ok(Plan::Effect {
            fault,
            scope: p.scope.clone(),
            at,
            effect: p.effect.clone(),
            command: p.command,
            handle: Self::handle(p),
        })
    }
    pub(crate) fn ready(
        &mut self,
        adapter: &mut impl Adapter,
        target: &str,
        boot: u64,
        observed: u64,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        if observed > at {
            return Err("Ready observation exceeds commit".into());
        }
        self.dispatch_waiting(adapter, target, boot, at)
    }
    /// Send the boot's guest commands that wait for nothing more, in
    /// admission order: readiness has come, and a heal queued behind its step
    /// goes once that step is acknowledged.
    pub(crate) fn dispatch_waiting(
        &mut self,
        adapter: &mut impl Adapter,
        target: &str,
        boot: u64,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        let mut plans = vec![];
        for (&fault, p) in &mut self.pending {
            if p.scope.is_machine(target, boot)
                && p.effect.guest_command()
                && p.command.is_none()
                && at <= p.deadline
                // Only the heal waits; its step goes first.
                && !(p.handle.as_ref())
                    .is_some_and(|h| h.healing() && self.handles.awaiting_start(h.record_id))
            {
                match Self::dispatch(adapter, fault, p, at) {
                    Ok(plan) => plans.push(plan),
                    Err(e) => {
                        return Err(self.fatal(Some(fault), Some(at), at, e.message));
                    }
                }
            }
        }
        Ok(plans)
    }
    /// Confirm a non-process mutation, or a matching process acknowledgment.
    /// Queue acceptance alone must never call this method.
    /// Revalidate a queued plan against the facts at its actual FIFO admission.
    /// None means an earlier terminal observation already settled this plan.
    pub(crate) fn dispatch_check(
        &self,
        adapter: &impl Adapter,
        fault: u32,
    ) -> Result<Option<Result<(), OperationError>>, Failure> {
        let Some(pending) = self.pending.get(&fault) else {
            return Ok(None);
        };
        if pending
            .handle
            .as_ref()
            .is_some_and(|p| self.handles.collision(p))
        {
            return Ok(Some(Err(error(
                ErrorCode::InvalidState,
                "duplicate active handle",
            ))));
        }
        // Links bind to no boot; their members stay admitted machines.
        let Some((target, boot)) = pending.scope.machine() else {
            return Ok(Some(Ok(())));
        };
        if pending.effect == Effect::Spawn {
            return Ok(Some(Ok(())));
        }
        let facts = match adapter.resolve(&Target::Key(target.to_owned())) {
            Ok(facts) => facts,
            Err(error) => return Ok(Some(Err(error))),
        };
        if facts.boot != boot {
            return Ok(Some(Err(error(
                ErrorCode::TargetUnavailable,
                "target boot ended before effect admission",
            ))));
        }
        if facts.physically_ended
            && facts.vm != Vm::Down
            && matches!(
                pending.effect,
                Effect::Pause
                    | Effect::Resume
                    | Effect::Shutdown
                    | Effect::Process(_)
                    | Effect::StepRealtime(_)
            )
        {
            return Ok(Some(Err(error(
                ErrorCode::TargetUnavailable,
                "target VM has exited",
            ))));
        }
        let allowed = match pending.effect {
            Effect::Pause => facts.vm == Vm::Up,
            Effect::Resume => facts.vm == Vm::Paused,
            Effect::Crash => facts.vm != Vm::Down,
            Effect::Reboot => facts.vm == Vm::Down,
            Effect::Shutdown => facts.vm != Vm::Down,
            Effect::Execution(_)
            | Effect::Spawn
            | Effect::Drift(_)
            | Effect::Fault { .. }
            | Effect::Unfault { .. } => true,
            // Commands already reserved for the application are decided by its
            // actual acknowledgment. Earlier commands can change process state.
            Effect::Process(_) | Effect::StepRealtime(_) => {
                facts.vm != Vm::Down && !facts.physically_ended
            }
            Effect::Partition
            | Effect::Unpartition
            | Effect::Shape(..)
            | Effect::Unshape
            | Effect::Select
            | Effect::Unselect => return Err("link effect bound to a machine".into()),
        };
        Ok(Some(if allowed {
            Ok(())
        } else {
            Err(error(
                ErrorCode::InvalidState,
                "target changed before effect admission",
            ))
        }))
    }
}
