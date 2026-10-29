//! Guest simulation-control call admission: queries, sleeps, trigger
//! arming and fault requests.
use super::*;

impl Service {
    /// Called only for a newly committed call. Transport owns session/request
    /// uniqueness. Admission uses the commit barrier, never the earlier byte time.
    pub(crate) fn admit(
        &mut self,
        adapter: &mut impl Adapter,
        source: String,
        id: SimulationControlId,
        observed: u64,
        now: u64,
        payload: &[u8],
    ) -> Result<Vec<Plan>, Failure> {
        self.admit_inner(adapter, (source, None), id, observed, now, payload)
    }
    /// Admit an explicitly boot-owned service call; the boot is carried through
    /// pending effects/sleep and into its reply, never inferred from a reused ID.
    pub(crate) fn admit_for_boot(
        &mut self,
        adapter: &mut impl Adapter,
        caller: (String, u64),
        id: SimulationControlId,
        observed: u64,
        now: u64,
        payload: &[u8],
    ) -> Result<Vec<Plan>, Failure> {
        self.admit_inner(
            adapter,
            (caller.0, Some(caller.1)),
            id,
            observed,
            now,
            payload,
        )
    }
    fn admit_inner(
        &mut self,
        adapter: &mut impl Adapter,
        caller: (String, Option<u64>),
        id: SimulationControlId,
        observed: u64,
        now: u64,
        payload: &[u8],
    ) -> Result<Vec<Plan>, Failure> {
        let (source, boot) = caller;
        self.valid()?;
        if observed > now {
            return Err("call observation exceeds commit".into());
        }
        let request = parse(payload).map_err(|e| format!("control protocol: {e:?}"))?;
        if request.id != id.request {
            return Err("control request identity mismatch".into());
        }
        let call = Call {
            boot,
            source,
            id,
            requested: observed,
            admitted: now,
        };
        let operation = request.operation.and_then(|op| {
            op.validate_at(now)?;
            Ok(op)
        });
        // A machine hold a block_request trigger installs or releases.
        let mut hold = None;
        let result = match operation {
            Err(e) => Err(e),
            Ok(Operation::Now {}) => Ok(json!({"now_ns":now})),
            Ok(Operation::Inspect {}) => adapter.inspect(
                &self
                    .handles
                    .active_handles()
                    .map_err(|e| format!("handles: {e:?}"))?,
                self.triggers.records(),
            ),
            Ok(Operation::Connections { from, to, after }) => {
                Self::connections(adapter, &from, &to, after)
            }
            Ok(Operation::Sleep { duration_ns }) => return self.sleep(call, now, duration_ns),
            Ok(Operation::ArmTrigger {
                source,
                marker: Some(marker),
                block_request: None,
                match_timeout_ns,
                fault,
            }) => self.arm_trigger(adapter, &source, marker, match_timeout_ns, fault, now),
            Ok(Operation::ArmTrigger {
                source,
                marker: None,
                block_request: Some(request),
                match_timeout_ns,
                fault,
            }) => self
                .arm_block_request(adapter, &source, request, match_timeout_ns, fault, now)
                .map(|(reply, plan)| {
                    hold = Some(plan);
                    reply
                }),
            Ok(Operation::ArmTrigger { .. }) => Err(OperationError::invalid(
                "trigger watches exactly one of a marker or a block_request",
            )),
            Ok(Operation::CancelTrigger { trigger_id }) => {
                self.cancel_trigger(trigger_id, now).inspect(|_| {
                    hold = self.release(trigger_id);
                })
            }
            Ok(
                op @ (Operation::Stop { .. }
                | Operation::Restart { .. }
                | Operation::PauseProcess { .. }
                | Operation::ResumeProcess { .. }
                | Operation::Pause { .. }
                | Operation::Resume { .. }
                | Operation::Crash { .. }
                | Operation::Reboot { .. }
                | Operation::Throttle { .. }
                | Operation::Heal { .. }
                | Operation::ClockSkew { .. }
                | Operation::ClockDrift { .. }
                | Operation::Spawn { .. }
                | Operation::Partition { .. }
                | Operation::Shape { .. }
                | Operation::DropConnection { .. }
                | Operation::DiskError { .. }
                | Operation::DiskLatency { .. }
                | Operation::Preempt { .. }),
            ) => match Self::require_capability(adapter, &op)
                .and_then(|()| Command::from_operation(op))
            {
                Err(e) => Err(e),
                Ok(command) => match self.prepare(
                    adapter,
                    AdmissionMode::Control,
                    Requester::Call {
                        source: call.source.clone(),
                        id: call.id,
                    },
                    Some(call.clone()),
                    command,
                    now,
                    PROCESS_EFFECT_TIMEOUT_NS,
                ) {
                    Ok((_, plans)) => return Ok(plans),
                    Err(PreparationError::Fatal(e)) => {
                        self.poison();
                        return Err(e);
                    }
                    Err(PreparationError::Rejected(e)) => Err(e),
                },
            },
        };
        let effect = (result.is_ok()
            || result
                .as_ref()
                .is_err_and(|e| e.code == ErrorCode::TargetUnavailable))
        .then_some(now);
        let mut plans: Vec<_> = hold.into_iter().collect();
        plans.push(self.reply(call, now, effect, result)?);
        Ok(plans)
    }
    /// Refuse a device fault the engine running these machines cannot carry
    /// out, after its arguments and before its target or handle.
    fn require_capability(adapter: &impl Adapter, op: &Operation) -> Result<(), OperationError> {
        match op.capability() {
            Some((name, capability)) => adapter.require_capability(name, capability),
            None => Ok(()),
        }
    }
    /// List the connections between two network members after `after`.
    fn connections(
        adapter: &impl Adapter,
        from: &Target,
        to: &Target,
        after: u64,
    ) -> Result<Value, OperationError> {
        let from = adapter.network_member(from)?;
        let to = adapter.network_member(to)?;
        if from == to {
            return Err(OperationError::invalid("connection endpoints must differ"));
        }
        let connections = adapter.connections(&from, &to, after)?;
        let next = connections.last().map_or(after, |c| c.incarnation);
        Ok(json!({"connections":connections,"next_after":next}))
    }
    /// Park `call` until its wake token is due; the reply follows the wake.
    fn sleep(&mut self, call: Call, now: u64, duration_ns: u64) -> Result<Vec<Plan>, Failure> {
        self.last_wake = self
            .last_wake
            .checked_add(1)
            .ok_or("sleep wake IDs exhausted")?;
        let token = self.last_wake;
        self.sleepers.insert(token, (call, now + duration_ns));
        Ok(vec![Plan::Wake {
            token,
            at: now + duration_ns,
        }])
    }
    /// Resolve a trigger's source, then its fault targets, and arm it.
    fn arm_trigger(
        &mut self,
        adapter: &impl Adapter,
        source: &Target,
        marker: Vec<u8>,
        match_timeout_ns: u64,
        fault: TriggerFault,
        now: u64,
    ) -> Result<Value, OperationError> {
        let source = adapter.resolve(source)?;
        let (action, target) = match fault {
            TriggerFault::Stop { target } => {
                let target = adapter.resolve(&target)?;
                (TriggerAction::Stop(target.name.clone()), Some(target.name))
            }
            TriggerFault::Partition { left, right } => {
                // The same membership rules as an untriggered partition.
                let (left, right) = Self::partition_sides(adapter, &left, &right)?;
                (TriggerAction::Partition { left, right }, None)
            }
            TriggerFault::Crash { .. } => {
                return Err(OperationError::invalid(
                    "only a block_request trigger crashes its machine",
                ))
            }
        };
        let id = self
            .triggers
            .arm(source.name.clone(), marker, now, match_timeout_ns, action)
            .map_err(|e| error(ErrorCode::InvalidState, e))?;
        let deadline = self.triggers.records()[id as usize - 1].match_deadline;
        self.notices.push(Notice::TriggerArmed {
            trigger: id,
            source: source.name,
            target,
            at: now,
            deadline,
        });
        Ok(json!({"trigger_id":id}))
    }
    /// Check the engine holds requests and the crash targets the source
    /// itself, then arm a block_request trigger and plan its machine hold.
    fn arm_block_request(
        &mut self,
        adapter: &impl Adapter,
        source: &Target,
        request: crate::oracle::BlockRequest,
        match_timeout_ns: u64,
        fault: TriggerFault,
        now: u64,
    ) -> Result<(Value, Plan), OperationError> {
        adapter.require_capability(
            "a block_request trigger",
            crate::driver::faults::Capability::HeldBlockRequest,
        )?;
        let source = adapter.resolve(source)?;
        let TriggerFault::Crash { target } = fault else {
            return Err(OperationError::invalid(
                "a block_request trigger's only fault is crash",
            ));
        };
        if adapter.resolve(&target)?.name != source.name {
            return Err(OperationError::invalid(
                "a block_request trigger crashes its own machine",
            ));
        }
        let id = self
            .triggers
            .arm_block_request(
                source.name.clone(),
                request.op,
                request.nth,
                now,
                match_timeout_ns,
            )
            .map_err(|e| error(ErrorCode::InvalidState, e))?;
        let until = self.triggers.records()[id as usize - 1].match_deadline;
        self.notices.push(Notice::TriggerArmed {
            trigger: id,
            source: source.name.clone(),
            target: Some(source.name.clone()),
            at: now,
            deadline: until,
        });
        let plan = Plan::Hold {
            source: source.name,
            trigger: id,
            op: request.op,
            nth: request.nth,
            until,
        };
        Ok((json!({"trigger_id":id}), plan))
    }
    fn cancel_trigger(&mut self, trigger_id: u32, now: u64) -> Result<Value, OperationError> {
        self.triggers
            .cancel(trigger_id, now)
            .map(|_| {
                self.notices.push(Notice::TriggerResolved {
                    trigger: trigger_id,
                    at: now,
                    status: crate::oracle::triggers::Status::Cancelled,
                    fault: None,
                });
                json!({})
            })
            .map_err(|e| error(ErrorCode::InvalidState, e))
    }
    pub(crate) fn wake(&mut self, token: u32, at: u64) -> Result<Plan, Failure> {
        self.valid()?;
        let (call, due) = self.sleepers.get(&token).ok_or("unknown sleep wake")?;
        if at < *due {
            return Err("early sleep wake".into());
        }
        let call = call.clone();
        self.sleepers.remove(&token);
        self.reply(call, at, Some(at), Ok(json!({})))
    }
}
