//! Fault settlement: confirmations, acknowledgments, rejections, ended
//! targets and fatal failures, with their notices and replies.
use super::*;

impl Service {
    /// A pre-effect failure with a no-mutation guarantee: never poison Handles.
    pub(crate) fn failed_without_effect(
        &mut self,
        fault: u32,
        at: u64,
        message: String,
    ) -> Result<Vec<Plan>, Failure> {
        self.finish_inner(
            fault,
            at,
            at,
            Err(error(ErrorCode::InvalidState, message)),
            Some(SettlementStatus::Failed),
        )
    }
    pub(crate) fn applied_network(
        &mut self,
        fault: u32,
        at: u64,
        network: Option<crate::driver::NetworkSettlement>,
    ) -> Result<Vec<Plan>, Failure> {
        if let Some(p) = self.pending.get_mut(&fault) {
            // Selection heal counts were frozen at admission, not sampled now.
            if p.network.is_none() {
                p.network = network;
            }
            if let Some(counts) = &p.network {
                p.result = serde_json::to_value(counts)
                    .map_err(|e| Failure::from(format!("network result: {e}")))?;
            }
        }
        self.applied(fault, at, at)
    }
    pub(crate) fn applied(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        if self
            .pending
            .get(&fault)
            .is_some_and(|p| p.effect.guest_command())
        {
            return Err("guest command completion requires acknowledgment".into());
        }
        self.finish(fault, observed, at, Ok(()))
    }
    // Keep the existing acknowledgment seam stable during service integration.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn acknowledged(
        &mut self,
        adapter: &impl Adapter,
        target: &str,
        boot: u64,
        id: NonZeroU32,
        result: CommandResult,
        observed: u64,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        // The actual tuple associates diagnostic evidence with a pending fault;
        // an unvalidated adapter fault ID must never retarget that evidence.
        let actual = self
            .pending
            .iter()
            .find(|(_, p)| p.scope.is_machine(target, boot) && p.command == Some(id))
            .map(|(&id, _)| id);
        let ownership = match adapter.command_owner(target, boot, id) {
            Ok(o) => o,
            Err(e) => return Err(self.fatal(actual, Some(observed), at, e)),
        };
        let (fault, action) = match ownership {
            CommandOwner::RetiredBoot | CommandOwner::RetiredApplication => return Ok(vec![]),
            CommandOwner::External if actual.is_none() => return Ok(vec![]),
            CommandOwner::External => {
                return Err(self.fatal(
                    actual,
                    Some(observed),
                    at,
                    "service command classified as external",
                ));
            }
            CommandOwner::Service { fault, action } => (fault, action),
        };
        if actual != Some(fault)
            || !self
                .pending
                .get(&fault)
                .is_some_and(|p| p.effect.command_kind() == Some(action))
        {
            return Err(self.fatal(
                actual,
                Some(observed),
                at,
                "miscorrelated service acknowledgment",
            ));
        }
        if result == CommandResult::Rejected && action == CommandKind::StepRealtime {
            // The kernel may refuse an admissible request (realtime below
            // CLOCK_MONOTONIC); nothing changed, so settle Rejected without
            // poisoning. Policy choice pending approval (process refusal is fatal).
            if self.pending[&fault]
                .dispatched
                .is_some_and(|sent| observed < sent)
            {
                return Err(self.fatal(
                    Some(fault),
                    Some(observed),
                    at,
                    "acknowledgment precedes dispatch",
                ));
            }
            return self.finish(
                fault,
                observed,
                at,
                Err(error(
                    ErrorCode::InvalidState,
                    "guest refused the realtime step",
                )),
            );
        }
        if result == CommandResult::Rejected {
            return Err(self.fatal(
                Some(fault),
                Some(observed),
                at,
                "guest rejected an admitted process effect",
            ));
        }
        if self.pending[&fault]
            .dispatched
            .is_some_and(|sent| observed < sent)
        {
            return Err(self.fatal(
                Some(fault),
                Some(observed),
                at,
                "acknowledgment precedes dispatch",
            ));
        }
        self.finish(fault, observed, at, Ok(()))
    }
    pub(crate) fn rejected(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
        reason: OperationError,
    ) -> Result<Vec<Plan>, Failure> {
        self.finish(fault, observed, at, Err(reason))
    }
    pub(super) fn finish(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
        result: Result<(), OperationError>,
    ) -> Result<Vec<Plan>, Failure> {
        self.finish_inner(fault, observed, at, result, None)
    }
    /// A realtime step that settles without taking effect takes the heal
    /// queued behind it along: the heal was never sent, and its handle
    /// bookkeeping is returned first so the step's record can go.
    fn finish_inner(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
        result: Result<(), OperationError>,
        forced: Option<SettlementStatus>,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        let queued = match result {
            Err(_) => self.queued_heal(fault),
            Ok(()) => None,
        };
        if let Some((_, plan)) = &queued {
            if let Err(e) = self.handles.rejected(plan) {
                return Err(self.fatal(
                    Some(fault),
                    Some(observed),
                    at,
                    format!("handle completion: {e:?}"),
                ));
            }
        }
        let mut plans = self.complete(fault, observed, at, result, forced, true)?;
        if let Some((heal, _)) = queued {
            let detail = format!("sequenced behind fault {fault}, which was not applied");
            let result = Err(error(ErrorCode::TargetUnavailable, detail));
            match self.complete(heal, at, at, result, None, false) {
                Ok(p) => plans.extend(p),
                Err(mut failure) => {
                    failure.retain_before(plans);
                    return Err(failure);
                }
            }
        }
        Ok(plans)
    }
    /// The pending heal queued behind step `fault`, with its handle plan.
    fn queued_heal(&self, fault: u32) -> Option<(u32, ExecutionPlan)> {
        let start = self.pending.get(&fault)?.handle.as_ref()?;
        if start.healing() || !self.handles.awaiting_start(start.record_id) {
            return None;
        }
        self.pending.iter().find_map(|(&id, p)| {
            p.handle
                .as_ref()
                .filter(|h| h.healing() && h.record_id == start.record_id)
                .map(|h| (id, h.clone()))
        })
    }
    fn complete(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
        result: Result<(), OperationError>,
        forced: Option<SettlementStatus>,
        handle_completion: bool,
    ) -> Result<Vec<Plan>, Failure> {
        let p = self
            .pending
            .get(&fault)
            .ok_or("unknown control completion")?;
        if observed > at || observed > p.deadline || observed < p.admitted {
            return Err(self.fatal(
                Some(fault),
                Some(observed),
                at,
                "effect completion outside admitted interval",
            ));
        }
        let status = forced.unwrap_or(match &result {
            Ok(()) => SettlementStatus::Applied,
            Err(e) if e.code == ErrorCode::TargetUnavailable => SettlementStatus::Unavailable,
            Err(_) => SettlementStatus::Rejected,
        });
        let detail = result
            .as_ref()
            .err()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| p.note.unwrap_or("confirmed effect").into());
        let settlement = Self::settlement(fault, p, Some(observed), at, status, detail);
        if let Some(handle) = p.handle.as_ref().filter(|_| handle_completion) {
            let completion = if result.is_ok() {
                self.handles.applied(handle)
            } else {
                self.handles.rejected(handle)
            };
            if let Err(e) = completion {
                return Err(self.fatal(
                    Some(fault),
                    Some(observed),
                    at,
                    format!("handle completion: {e:?}"),
                ));
            }
        }
        let p = self.pending.remove(&fault).unwrap();
        self.notices.push(Notice::Settled(settlement.clone()));
        let mut plans = vec![Plan::Settled(Box::new(settlement))];
        if let Some(call) = p.call {
            let effect = (result.is_ok()
                || result
                    .as_ref()
                    .is_err_and(|e| e.code == ErrorCode::TargetUnavailable))
            .then_some(observed);
            match self.reply(call, at, effect, result.map(|_| p.result)) {
                Ok(reply) => plans.push(reply),
                Err(mut failure) => {
                    if let Plan::Settled(s) = plans.remove(0) {
                        failure.settlements.push(*s);
                    }
                    self.poison();
                    return Err(failure);
                }
            }
        }
        Ok(plans)
    }
    /// Exact occurrence notification, including silent crash/reset. Never derive
    /// this timestamp from a later adopted Exited.reached boundary.
    pub(crate) fn ended(
        &mut self,
        target: &str,
        boot: u64,
        observed: u64,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        let plans = self.end_processes(target, boot, observed, at, true)?;
        // Settled realtime steps ended with this kernel; drift persists.
        self.handles.machine_ended(target);
        Ok(plans)
    }
    /// Natural application exit retires operations on that old group. A queued
    /// Start belongs to the next application and must still receive its Ack.
    pub(crate) fn application_ended(
        &mut self,
        target: &str,
        boot: u64,
        observed: u64,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        self.end_processes(target, boot, observed, at, false)
    }
    fn end_processes(
        &mut self,
        target: &str,
        boot: u64,
        observed: u64,
        at: u64,
        include_start: bool,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        let ids: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                p.scope.is_machine(target, boot)
                    && match p.effect {
                        Effect::Process(action) => {
                            include_start || action != ApplicationCommand::Start
                        }
                        // Steps are kernel work: application exit keeps them.
                        Effect::StepRealtime(_) => include_start,
                        _ => false,
                    }
            })
            .map(|(&id, _)| id)
            .collect();
        let mut plans = vec![];
        for id in ids {
            // A queued heal may already have settled with its step.
            if !self.pending.contains_key(&id) {
                continue;
            }
            match self.finish(
                id,
                observed,
                at,
                Err(error(
                    ErrorCode::TargetUnavailable,
                    "target ended before effect completion",
                )),
            ) {
                Ok(p) => plans.extend(p),
                Err(mut failure) => {
                    failure.retain_before(plans);
                    return Err(failure);
                }
            }
        }
        Ok(plans)
    }
    /// A guest command still pending at its deadline fails the run, except
    /// one never sent because its rebooted machine was not ready again in
    /// time: like one for a down machine, it is unavailable. One waiting for
    /// a first boot that never becomes ready still fails.
    ///
    /// Commands admitted after it for that boot were checked against its
    /// effect and would follow it, so the unsent ones are unavailable with it:
    /// a restart never reaches the guest without its stop.
    ///
    /// A heal still queued behind its unacknowledged step is unavailable too,
    /// leaving the step for a later heal.
    pub(crate) fn deadline(&mut self, fault: u32, at: u64) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        if let Some(p) = self.pending.get(&fault) {
            if at < p.deadline {
                return Err("early effect deadline".into());
            }
            let queued = (p.handle.as_ref())
                .is_some_and(|h| h.healing() && self.handles.awaiting_start(h.record_id));
            if queued && p.command.is_none() {
                return self.finish(
                    fault,
                    at,
                    at,
                    Err(error(
                        ErrorCode::TargetUnavailable,
                        "realtime step was not acknowledged in time",
                    )),
                );
            }
            if p.command.is_none() && p.scope.machine().is_some_and(|(_, boot)| boot > 0) {
                let behind: Vec<u32> = (self.pending.range(fault + 1..))
                    .filter(|(_, q)| {
                        q.scope == p.scope && q.effect.guest_command() && q.command.is_none()
                    })
                    .map(|(&id, _)| id)
                    .collect();
                let mut plans = self.finish(
                    fault,
                    at,
                    at,
                    Err(error(
                        ErrorCode::TargetUnavailable,
                        "target machine was not ready again in time",
                    )),
                )?;
                for id in behind {
                    if !self.pending.contains_key(&id) {
                        continue;
                    }
                    let detail = format!("sequenced behind fault {fault}, which was not delivered");
                    match self.finish(id, at, at, Err(error(ErrorCode::TargetUnavailable, detail)))
                    {
                        Ok(p) => plans.extend(p),
                        Err(mut failure) => {
                            failure.retain_before(plans);
                            return Err(failure);
                        }
                    }
                }
                return Ok(plans);
            }
            return Err(self.fatal(Some(fault), Some(at), at, "process effect deadline expired"));
        }
        Ok(vec![])
    }
    fn settlement(
        fault: u32,
        p: &Pending,
        observed: Option<u64>,
        commit: u64,
        status: SettlementStatus,
        detail: String,
    ) -> Settlement {
        Settlement {
            record_id: p.handle.as_ref().map(|h| h.record_id),
            network: p.network.clone(),
            fault,
            requester: p.requester.clone(),
            scope: p.scope.clone(),
            action: Self::action(p),
            requested_at: Self::requested_at(p),
            admitted_at: p.admitted,
            outcome_at: observed,
            committed_at: commit,
            status,
            handle: Self::handle(p),
            command: p.command,
            detail,
        }
    }
    pub(super) fn action(p: &Pending) -> FaultAction {
        if p.healing {
            FaultAction::Heal
        } else {
            FaultAction::Effect(p.effect.clone())
        }
    }
    pub(super) fn requested_at(p: &Pending) -> u64 {
        p.call.as_ref().map_or(p.admitted, |c| c.requested)
    }
    pub(super) fn handle(p: &Pending) -> Option<String> {
        p.handle.as_ref().map(|h| h.handle.to_string())
    }
    pub(super) fn fatal(
        &mut self,
        fault: Option<u32>,
        observed: Option<u64>,
        commit: u64,
        detail: impl Into<String>,
    ) -> Failure {
        let mut message = detail.into();
        let settlements = fault
            .and_then(|id| {
                self.pending.remove(&id).map(|p| {
                    let occurrence = observed.filter(|t| {
                        *t >= p.admitted
                            && *t <= commit
                            && p.dispatched.is_none_or(|sent| *t >= sent)
                    });
                    if occurrence != observed {
                        if let Some(raw) = observed {
                            message.push_str(&format!("; invalid receipt_time_ns={raw}"));
                        }
                    }
                    Self::settlement(
                        id,
                        &p,
                        occurrence,
                        commit,
                        SettlementStatus::Failed,
                        message.clone(),
                    )
                })
            })
            .into_iter()
            .collect::<Vec<_>>();
        self.notices
            .extend(settlements.iter().cloned().map(Notice::Settled));
        self.poison();
        Failure {
            message,
            settlements,
        }
    }
    /// Execution/Driver failure has no rollback guarantee. Unlike rejected(),
    /// this poisons service/ledger and returns Failed evidence, never a reply.
    pub(crate) fn execution_failed(
        &mut self,
        fault: u32,
        observed: Option<u64>,
        commit: u64,
        detail: String,
    ) -> Failure {
        if self.poisoned {
            return "control service poisoned".into();
        }
        self.fatal(Some(fault), observed, commit, detail)
    }
    pub(super) fn poison(&mut self) {
        self.poisoned = true;
        self.handles.execution_failed();
    }
}
