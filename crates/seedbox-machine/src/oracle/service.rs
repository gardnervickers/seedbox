//! Private control policy. Plans are executed by the owner of the single Driver
//! timeline; returning a plan never confirms its effect. Facts, command IDs and
//! first terminal-transition times must come from that owner, including changes
//! made outside this service. Machine snapshots alone do not snapshot this state.
use super::{
    handles::{ExecutionPlan, Handles},
    triggers::Triggers,
    *,
};
use crate::{
    execution::ExecutionConfig,
    pv::{ApplicationCommand, CommandResult, SimulationControlId},
};

/// Retained control-policy lifecycle, independent of physical Machine exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Vm {
    Up,
    Paused,
    Down,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Process {
    Running,
    Stopped,
    Frozen,
}
#[derive(Clone, Debug)]
pub(crate) struct Facts {
    pub name: String,
    pub boot: u64,
    pub vm: Vm,
    /// Process eligibility: a natural application exit permits another Start.
    /// The observation projection separately retains its actual exit outcome.
    pub process: Process,
    pub physically_ended: bool,
    pub ready: bool,
    pub execution: ExecutionConfig,
}
/// Authoritative stopped-state view, not an output-derived mirror. Inspection
/// must supply the complete legacy projection, or explicitly return Unsupported.
/// Lifecycle/process policy follows confirmed control admissions/completions;
/// physical exit is tracked separately. Natural application exit keeps the
/// machine Up and makes its process eligible for Start, without a commanded Stop. Physical exit preflight nevertheless rejects Pause/Resume and all
/// process controls before policy validation, matching the legacy API wrapper.
/// Logical Down excludes this preflight: new Machine may retain an Exited
/// reason after Crash, while legacy Crash installed a fresh unrun machine.
/// Down process faults therefore still admit and settle Unavailable regardless
/// of physical state. Crash/Reboot validate retained lifecycle independently.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CommandOwner {
    External,
    RetiredBoot,
    RetiredApplication,
    Service {
        fault: u32,
        action: ApplicationCommand,
    },
}
pub(crate) trait Adapter {
    fn resolve(&self, target: &Target) -> Result<Facts, OperationError>;
    /// Reserve from the shared boot allocator, excluding even future scheduled
    /// external IDs. Reservation is not machine acceptance: command_owner must
    /// reject a receipt until the owner actually admits this plan.
    fn command_id(
        &mut self,
        target: &str,
        boot: u64,
        fault: u32,
        action: ApplicationCommand,
    ) -> Result<NonZeroU32, String>;
    /// Authoritative admitted-command table, including external commands and
    /// retired boot policy. Unknown/current miscorrelation is an error.
    fn command_owner(
        &self,
        target: &str,
        boot: u64,
        id: NonZeroU32,
    ) -> Result<CommandOwner, String>;
    fn inspect(
        &self,
        handles: &[String],
        triggers: &[super::triggers::Record],
    ) -> Result<Value, OperationError>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Pause,
    Resume,
    Crash,
    Reboot,
    Execution(ExecutionConfig),
    Process(ApplicationCommand),
}
/// Terminal observations are handed to the timeline/history owner once. No
/// duplicate unbounded history is kept here. Outcome time is not applied time:
/// only Applied proves a mutation; Unavailable and Failed never do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettlementStatus {
    Applied,
    Unavailable,
    Rejected,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FaultAction {
    Effect(Effect),
    Heal,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settlement {
    pub fault: u32,
    pub target: String,
    pub boot: u64,
    pub action: FaultAction,
    pub requested_at: u64,
    pub admitted_at: u64,
    pub outcome_at: Option<u64>,
    pub committed_at: u64,
    pub status: SettlementStatus,
    pub handle: Option<String>,
    pub command: Option<NonZeroU32>,
    pub detail: String,
}
/// Fatal paths must return already-known settlements as well as the error.
/// The caller publishes these immutable facts and terminates; no rollback or
/// success reply is fabricated after an uncertain effect failure.
#[derive(Debug)]
pub(crate) struct Failure {
    pub message: String,
    pub settlements: Vec<Settlement>,
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            settlements: vec![],
        }
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl Failure {
    fn retain_before(&mut self, plans: Vec<Plan>) {
        let mut earlier: Vec<_> = plans
            .into_iter()
            .filter_map(|p| {
                if let Plan::Settled(s) = p {
                    Some(s)
                } else {
                    None
                }
            })
            .collect();
        earlier.append(&mut self.settlements);
        self.settlements = earlier;
    }
}
/// Admission policy is independent of whether work has an RPC reply destination.
/// Ordinary controls apply the API availability preflight; bare trigger faults
/// use the underlying fault admission path and can settle Unavailable.
#[derive(Clone, Copy)]
enum AdmissionMode {
    Control,
    Trigger,
}

enum PreparationError {
    Rejected(OperationError),
    Fatal(Failure),
}
impl From<OperationError> for PreparationError {
    fn from(e: OperationError) -> Self {
        Self::Rejected(e)
    }
}
impl From<Failure> for PreparationError {
    fn from(e: Failure) -> Self {
        Self::Fatal(e)
    }
}
#[derive(Clone, Debug)]
pub(crate) enum Plan {
    Settled(Settlement),
    Effect {
        fault: u32,
        target: String,
        boot: u64,
        at: u64,
        effect: Effect,
        command: Option<NonZeroU32>,
    },
    /// Owner schedules this token on its timeline, even if the caller disappears.
    Wake {
        token: u32,
        at: u64,
    },
    Deadline {
        fault: u32,
        at: u64,
    },
    Reply {
        source: String,
        id: SimulationControlId,
        at: u64,
        payload: Vec<u8>,
    },
}
#[derive(Clone)]
struct Call {
    source: String,
    id: SimulationControlId,
    requested: u64,
    admitted: u64,
}
#[derive(Clone)]
struct Pending {
    call: Option<Call>,
    target: String,
    boot: u64,
    effect: Effect,
    command: Option<NonZeroU32>,
    dispatched: Option<u64>,
    deadline: u64,
    admitted: u64,
    handle: Option<ExecutionPlan>,
    result: Value,
    healing: bool,
}
#[derive(Clone, Default)]
pub(crate) struct Service {
    handles: Handles,
    triggers: Triggers,
    last: u32,
    last_wake: u32,
    pending: BTreeMap<u32, Pending>,
    sleepers: BTreeMap<u32, (Call, u64)>,
    poisoned: bool,
}
fn error(code: ErrorCode, message: impl Into<String>) -> OperationError {
    OperationError {
        code,
        message: message.into(),
        handle: None,
    }
}
impl Service {
    fn valid(&self) -> Result<(), Failure> {
        if self.poisoned {
            Err("control service poisoned".into())
        } else {
            Ok(())
        }
    }
    fn token(&mut self) -> Result<u32, Failure> {
        self.last = self
            .last
            .checked_add(1)
            .ok_or("control operation IDs exhausted")?;
        Ok(self.last)
    }
    fn reply(
        call: Call,
        now: u64,
        effect: Option<u64>,
        result: Result<Value, OperationError>,
    ) -> Result<Plan, Failure> {
        if now < call.admitted {
            return Err("reply precedes admission".into());
        }
        let payload = encode(
            call.id.request,
            ReplyMetadata {
                requested_at_ns: call.requested,
                admitted_at_ns: call.admitted,
                effect_at_ns: effect,
                replied_at_ns: now,
            },
            result.as_ref().map_err(Clone::clone),
        )
        .map_err(|e| format!("reply encoding: {e:?}"))?;
        Ok(Plan::Reply {
            source: call.source,
            id: call.id,
            at: now,
            payload,
        })
    }
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
        self.valid()?;
        if observed > now {
            return Err("call observation exceeds commit".into());
        }
        let request = parse(payload).map_err(|e| format!("control protocol: {e:?}"))?;
        if request.id != id.request {
            return Err("control request identity mismatch".into());
        }
        let call = Call {
            source,
            id,
            requested: observed,
            admitted: now,
        };
        let operation = request.operation.and_then(|op| {
            op.validate_at(now)?;
            Ok(op)
        });
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
            Ok(Operation::Sleep { duration_ns }) => {
                self.last_wake = self
                    .last_wake
                    .checked_add(1)
                    .ok_or("sleep wake IDs exhausted")?;
                let token = self.last_wake;
                self.sleepers.insert(token, (call, now + duration_ns));
                return Ok(vec![Plan::Wake {
                    token,
                    at: now + duration_ns,
                }]);
            }
            Ok(Operation::ArmTrigger {
                source,
                marker,
                match_timeout_ns,
                fault: TriggerFault::Stop { target },
            }) => adapter.resolve(&source).and_then(|source| {
                adapter.resolve(&target).and_then(|target| {
                    self.triggers
                        .arm(source.name, marker, now, match_timeout_ns, target.name)
                        .map(|id| json!({"trigger_id":id}))
                        .map_err(|e| error(ErrorCode::InvalidState, e))
                })
            }),
            Ok(Operation::CancelTrigger { trigger_id }) => self
                .triggers
                .cancel(trigger_id, now)
                .map(|_| json!({}))
                .map_err(|e| error(ErrorCode::InvalidState, e)),
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
                | Operation::Heal { .. }),
            ) => match self.prepare(adapter, AdmissionMode::Control, Some(call.clone()), op, now) {
                Ok((_, plans)) => return Ok(plans),
                Err(PreparationError::Fatal(e)) => {
                    self.poison();
                    return Err(e);
                }
                Err(PreparationError::Rejected(e)) => Err(e),
            },
            Ok(_) => Err(OperationError::unsupported()),
        };
        let effect = (result.is_ok()
            || result
                .as_ref()
                .is_err_and(|e| e.code == ErrorCode::TargetUnavailable))
        .then_some(now);
        Ok(vec![Self::reply(call, now, effect, result)?])
    }
    fn prepare(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        call: Option<Call>,
        op: Operation,
        now: u64,
    ) -> Result<(u32, Vec<Plan>), PreparationError> {
        let mut handle = None;
        let (facts, effect) = match op {
            Operation::Heal { handle: text } => {
                let plan = self
                    .handles
                    .heal(&text)
                    .map_err(|e| error(ErrorCode::InvalidState, format!("heal: {e:?}")))?;
                let facts = match adapter.resolve(&Target::Machine(plan.target.clone())) {
                    Ok(f) => f,
                    Err(e) => {
                        self.handles
                            .rejected(&plan)
                            .expect("matching local reservation");
                        return Err(e.into());
                    }
                };
                let effect = Effect::Execution(plan.config);
                handle = Some(plan);
                (facts, effect)
            }
            op => {
                let (target, effect) = match op {
                    Operation::Stop { target } => {
                        (target, Effect::Process(ApplicationCommand::Stop))
                    }
                    Operation::Restart { target } => {
                        (target, Effect::Process(ApplicationCommand::Start))
                    }
                    Operation::PauseProcess { target } => {
                        (target, Effect::Process(ApplicationCommand::Pause))
                    }
                    Operation::ResumeProcess { target } => {
                        (target, Effect::Process(ApplicationCommand::Resume))
                    }
                    Operation::Pause { target } => (target, Effect::Pause),
                    Operation::Resume { target } => (target, Effect::Resume),
                    Operation::Crash { target } => (target, Effect::Crash),
                    Operation::Reboot { target } => (target, Effect::Reboot),
                    Operation::Throttle {
                        target,
                        factor_permille,
                    } => {
                        let facts = adapter.resolve(&target)?;
                        let config = ExecutionConfig::new(factor_permille as u32)
                            .map_err(|_| OperationError::invalid("invalid execution factor"))?;
                        let plan =
                            self.handles
                                .throttle(facts.name.clone(), config)
                                .map_err(|e| {
                                    error(ErrorCode::InvalidState, format!("throttle: {e:?}"))
                                })?;
                        handle = Some(plan);
                        return self.install(
                            adapter,
                            mode,
                            call,
                            facts,
                            Effect::Execution(config),
                            handle,
                            now,
                        );
                    }
                    _ => unreachable!(),
                };
                (adapter.resolve(&target)?, effect)
            }
        };
        let heal = matches!(effect, Effect::Execution(_)) && handle.is_some();
        let result = self.install(adapter, mode, call, facts, effect, handle, now)?;
        if heal {
            let p = self.pending.get_mut(&result.0).unwrap();
            p.result = json!({});
            p.healing = true;
        }
        Ok(result)
    }
    fn install(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        call: Option<Call>,
        facts: Facts,
        effect: Effect,
        handle: Option<ExecutionPlan>,
        now: u64,
    ) -> Result<(u32, Vec<Plan>), PreparationError> {
        // The control API's physical-availability preflight precedes Faults'
        // policy validation (world_windowed::admit_control/reject_unavailable).
        // Trigger admission bypasses this control-only wrapper.
        if matches!(mode, AdmissionMode::Control)
            && facts.physically_ended
            && facts.vm != Vm::Down
            && matches!(effect, Effect::Pause | Effect::Resume | Effect::Process(_))
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
            Effect::Execution(_) => true,
        };
        if !allowed {
            return Err(error(ErrorCode::InvalidState, "target state rejects operation").into());
        }
        let fault = self.token()?;
        let deadline = now
            .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
            .ok_or_else(|| OperationError::invalid("effect deadline overflow"))?;
        let pending = Pending {
            call,
            target: facts.name,
            boot: facts.boot,
            effect,
            command: None,
            dispatched: None,
            deadline,
            admitted: now,
            result: handle
                .as_ref()
                .map(|h| json!({"handle":h.handle.to_string()}))
                .unwrap_or_else(|| json!({})),
            handle,
            healing: false,
        };
        let unavailable = matches!(pending.effect, Effect::Process(_))
            && (facts.vm == Vm::Down || facts.physically_ended);
        self.pending.insert(fault, pending);
        let mut plans = vec![];
        if !unavailable
            && (!matches!(self.pending[&fault].effect, Effect::Process(_)) || facts.ready)
        {
            match Self::dispatch(adapter, fault, self.pending.get_mut(&fault).unwrap(), now) {
                Ok(plan) => plans.push(plan),
                Err(e) => return Err(self.fatal(Some(fault), Some(now), now, e.message).into()),
            }
        }
        if matches!(self.pending[&fault].effect, Effect::Process(_)) {
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
    fn dispatch(
        adapter: &mut impl Adapter,
        fault: u32,
        p: &mut Pending,
        at: u64,
    ) -> Result<Plan, Failure> {
        if let Effect::Process(action) = p.effect {
            p.command = Some(adapter.command_id(&p.target, p.boot, fault, action)?);
            p.dispatched = Some(at);
        }
        Ok(Plan::Effect {
            fault,
            target: p.target.clone(),
            boot: p.boot,
            at,
            effect: p.effect.clone(),
            command: p.command,
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
        let mut plans = vec![];
        for (&fault, p) in &mut self.pending {
            if p.target == target
                && p.boot == boot
                && matches!(p.effect, Effect::Process(_))
                && p.command.is_none()
                && at <= p.deadline
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
        let facts = match adapter.resolve(&Target::Machine(pending.target.clone())) {
            Ok(facts) => facts,
            Err(error) => return Ok(Some(Err(error))),
        };
        if facts.boot != pending.boot {
            return Ok(Some(Err(error(
                ErrorCode::TargetUnavailable,
                "target boot ended before effect admission",
            ))));
        }
        if facts.physically_ended
            && facts.vm != Vm::Down
            && matches!(
                pending.effect,
                Effect::Pause | Effect::Resume | Effect::Process(_)
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
            Effect::Execution(_) => true,
            // Commands already reserved for the application are decided by its
            // actual acknowledgment. Earlier commands can change process state.
            Effect::Process(_) => facts.vm != Vm::Down && !facts.physically_ended,
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
    pub(crate) fn applied(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        if self
            .pending
            .get(&fault)
            .is_some_and(|p| matches!(p.effect, Effect::Process(_)))
        {
            return Err("process completion requires acknowledgment".into());
        }
        self.finish(fault, observed, at, Ok(()))
    }
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
            .find(|(_, p)| p.target == target && p.boot == boot && p.command == Some(id))
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
                .is_some_and(|p| p.effect == Effect::Process(action))
        {
            return Err(self.fatal(
                actual,
                Some(observed),
                at,
                "miscorrelated service acknowledgment",
            ));
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
    fn finish(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
        result: Result<(), OperationError>,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
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
        let status = match &result {
            Ok(()) => SettlementStatus::Applied,
            Err(e) if e.code == ErrorCode::TargetUnavailable => SettlementStatus::Unavailable,
            Err(_) => SettlementStatus::Rejected,
        };
        let detail = result
            .as_ref()
            .err()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "confirmed effect".into());
        let settlement = Self::settlement(fault, p, Some(observed), at, status, detail);
        if let Some(handle) = &p.handle {
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
        let mut plans = vec![Plan::Settled(settlement)];
        if let Some(call) = p.call {
            let effect = (result.is_ok()
                || result
                    .as_ref()
                    .is_err_and(|e| e.code == ErrorCode::TargetUnavailable))
            .then_some(observed);
            match Self::reply(call, at, effect, result.map(|_| p.result)) {
                Ok(reply) => plans.push(reply),
                Err(mut failure) => {
                    if let Plan::Settled(s) = plans.remove(0) {
                        failure.settlements.push(s);
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
        self.end_processes(target, boot, observed, at, true)
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
                p.target == target && p.boot == boot && matches!(p.effect, Effect::Process(action) if include_start || action!=ApplicationCommand::Start)
            })
            .map(|(&id, _)| id)
            .collect();
        let mut plans = vec![];
        for id in ids {
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
    pub(crate) fn wake(&mut self, token: u32, at: u64) -> Result<Plan, Failure> {
        self.valid()?;
        let (call, due) = self.sleepers.get(&token).ok_or("unknown sleep wake")?;
        if at < *due {
            return Err("early sleep wake".into());
        }
        let call = call.clone();
        self.sleepers.remove(&token);
        Self::reply(call, at, Some(at), Ok(json!({})))
    }

    pub(crate) fn deadline(&mut self, fault: u32, at: u64) -> Result<(), Failure> {
        self.valid()?;
        if let Some(p) = self.pending.get(&fault) {
            if at < p.deadline {
                return Err("early effect deadline".into());
            }
            return Err(self.fatal(Some(fault), Some(at), at, "process effect deadline expired"));
        }
        Ok(())
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
            fault,
            target: p.target.clone(),
            boot: p.boot,
            action: if p.healing {
                FaultAction::Heal
            } else {
                FaultAction::Effect(p.effect.clone())
            },
            requested_at: p.call.as_ref().map_or(p.admitted, |c| c.requested),
            admitted_at: p.admitted,
            outcome_at: observed,
            committed_at: commit,
            status,
            handle: p.handle.as_ref().map(|h| h.handle.to_string()),
            command: p.command,
            detail,
        }
    }
    fn fatal(
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
            .collect();
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
    fn poison(&mut self) {
        self.poisoned = true;
        self.handles.execution_failed();
    }
    pub(crate) fn committed_bytes(
        &mut self,
        source: &str,
        observed: u64,
        bytes: &[u8],
        commit: u64,
    ) -> Result<(), Failure> {
        self.valid()?;
        self.triggers
            .committed(source, observed, bytes, commit)
            .map(|_| ())
            .map_err(Into::into)
    }
    pub(crate) fn trigger_effects(
        &mut self,
        adapter: &mut impl Adapter,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        let mut plans = vec![];
        for effect in self.triggers.due(at) {
            let result = self.prepare(
                adapter,
                AdmissionMode::Trigger,
                None,
                Operation::Stop {
                    target: Target::Machine(effect.target),
                },
                at,
            );
            match result {
                Ok((id, p)) => {
                    self.triggers.resolve(effect.trigger_id, at, Ok(id))?;
                    plans.extend(p);
                }
                Err(PreparationError::Rejected(e)) => {
                    self.triggers
                        .resolve(effect.trigger_id, at, Err(e.message))?
                }
                Err(PreparationError::Fatal(mut e)) => {
                    // A preparation failure after policy admission can already
                    // carry the new fault's Failed settlement. Fired records
                    // admission, not a successful guest effect.
                    if let Some(settlement) = e.settlements.first() {
                        self.triggers
                            .resolve(effect.trigger_id, at, Ok(settlement.fault))?;
                    }
                    e.retain_before(plans);
                    return Err(e);
                }
            }
        }
        Ok(plans)
    }
    pub(crate) fn source_reset(&mut self, source: &str) -> Result<(), Failure> {
        self.valid()?;
        self.triggers.reset_source(source).map_err(Into::into)
    }
    pub(crate) fn expire_triggers(&mut self, at: u64) {
        self.triggers.expire(at);
    }
    pub(crate) fn cut_off_triggers(&mut self, at: u64) {
        self.triggers.cut_off(at);
    }
    pub(crate) fn trigger_boundary(&self) -> Option<u64> {
        self.triggers.boundary()
    }
}
#[cfg(test)]
mod tests;
