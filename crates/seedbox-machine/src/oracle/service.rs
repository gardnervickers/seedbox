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
    // Retained for upcoming live execution-setting control.
    #[allow(dead_code)]
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
    /// Reserve a declared machine once, without building it or drawing entropy.
    fn reserve_spawn(&mut self, _: &str) -> Result<(), OperationError> {
        Err(OperationError::unsupported())
    }
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
    Spawn,
    Pause,
    Resume,
    Crash,
    Reboot,
    /// Latch graceful shutdown of the current boot (host inputs only).
    Shutdown,
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
/// Who requested an admitted fault: a guest control call, a matched trigger
/// or a host input identified by its validated key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Requester {
    Call {
        source: String,
        id: SimulationControlId,
    },
    Trigger(u32),
    Input(String),
}
/// A fault request after operation parsing, independent of its requester.
/// Guest operations convert through `from_operation`; host inputs construct
/// one directly, so host-only effects (Shutdown) never appear on the wire.
#[derive(Clone, Debug)]
pub(crate) enum Command {
    Effect {
        target: Target,
        effect: Effect,
    },
    Throttle {
        target: Target,
        config: ExecutionConfig,
    },
    Heal {
        handle: String,
    },
    /// Reserve and later build a declared machine by name.
    Spawn {
        machine: String,
    },
}
impl Command {
    fn from_operation(op: Operation) -> Result<Self, OperationError> {
        let effect = |target, effect| Ok(Self::Effect { target, effect });
        match op {
            Operation::Stop { target } => effect(target, Effect::Process(ApplicationCommand::Stop)),
            Operation::Restart { target } => {
                effect(target, Effect::Process(ApplicationCommand::Start))
            }
            Operation::PauseProcess { target } => {
                effect(target, Effect::Process(ApplicationCommand::Pause))
            }
            Operation::ResumeProcess { target } => {
                effect(target, Effect::Process(ApplicationCommand::Resume))
            }
            Operation::Pause { target } => effect(target, Effect::Pause),
            Operation::Resume { target } => effect(target, Effect::Resume),
            Operation::Crash { target } => effect(target, Effect::Crash),
            Operation::Reboot { target } => effect(target, Effect::Reboot),
            Operation::Throttle {
                target,
                factor_permille,
            } => Ok(Self::Throttle {
                target,
                config: u32::try_from(factor_permille)
                    .ok()
                    .and_then(|factor| ExecutionConfig::new(factor).ok())
                    .ok_or_else(|| OperationError::invalid("invalid execution factor"))?,
            }),
            Operation::Heal { handle } => Ok(Self::Heal { handle }),
            Operation::Spawn { machine } => Ok(Self::Spawn { machine }),
            _ => Err(OperationError::unsupported()),
        }
    }
}
/// Policy admission of a fault. Admission is not completion; a later
/// Settlement reports the outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Admitted {
    pub fault: u32,
    pub requester: Requester,
    pub target: String,
    pub boot: u64,
    pub action: FaultAction,
    pub handle: Option<String>,
    pub requested_at: u64,
    pub admitted_at: u64,
}
/// Incremental control facts in the order this service confirmed them. The
/// timeline owner takes them after every call, including failed calls, and
/// publishes them; this service keeps no history of drained notices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Admitted(Admitted),
    Settled(Settlement),
    TriggerArmed {
        trigger: u32,
        source: String,
        target: String,
        at: u64,
        deadline: u64,
    },
    /// `status` is a final trigger status, never Armed or Matched.
    TriggerResolved {
        trigger: u32,
        at: u64,
        status: super::triggers::Status,
        fault: Option<u32>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settlement {
    pub fault: u32,
    pub requester: Requester,
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
    requester: Requester,
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
    notices: Vec<Notice>,
}
fn error(code: ErrorCode, message: impl Into<String>) -> OperationError {
    OperationError {
        code,
        message: message.into(),
        handle: None,
    }
}
impl Service {
    /// Transfer confirmed facts once, in confirmation order.
    pub(crate) fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }
    /// Describe a pending fault as it was admitted, for a pre-effect hold.
    pub(crate) fn pending_admitted(&self, fault: u32) -> Option<Admitted> {
        let p = self.pending.get(&fault)?;
        Some(Admitted {
            fault,
            requester: p.requester.clone(),
            target: p.target.clone(),
            boot: p.boot,
            action: Self::action(p),
            handle: Self::handle(p),
            requested_at: Self::requested_at(p),
            admitted_at: p.admitted,
        })
    }
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
                    let id = self
                        .triggers
                        .arm(
                            source.name.clone(),
                            marker,
                            now,
                            match_timeout_ns,
                            target.name.clone(),
                        )
                        .map_err(|e| error(ErrorCode::InvalidState, e))?;
                    let deadline = self.triggers.records()[id as usize - 1].match_deadline;
                    self.notices.push(Notice::TriggerArmed {
                        trigger: id,
                        source: source.name,
                        target: target.name,
                        at: now,
                        deadline,
                    });
                    Ok(json!({"trigger_id":id}))
                })
            }),
            Ok(Operation::CancelTrigger { trigger_id }) => self
                .triggers
                .cancel(trigger_id, now)
                .map(|_| {
                    self.notices.push(Notice::TriggerResolved {
                        trigger: trigger_id,
                        at: now,
                        status: super::triggers::Status::Cancelled,
                        fault: None,
                    });
                    json!({})
                })
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
                | Operation::Heal { .. }
                | Operation::Spawn { .. }),
            ) => match Command::from_operation(op) {
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
            Ok(_) => Err(OperationError::unsupported()),
        };
        let effect = (result.is_ok()
            || result
                .as_ref()
                .is_err_and(|e| e.code == ErrorCode::TargetUnavailable))
        .then_some(now);
        Ok(vec![Self::reply(call, now, effect, result)?])
    }
    /// Admit a host input through the ordinary-control path at its due time.
    /// The outer result is fatal failure; the inner result is admission or
    /// policy rejection. A rejection allocates no fault ID and produces no
    /// reply; no guest call is fabricated. `deadline_ns` bounds process
    /// dispatch and acknowledgment after admission.
    pub(crate) fn admit_input(
        &mut self,
        adapter: &mut impl Adapter,
        key: String,
        command: Command,
        now: u64,
        deadline_ns: u64,
    ) -> Result<Result<(u32, Vec<Plan>), OperationError>, Failure> {
        self.valid()?;
        match self.prepare(
            adapter,
            AdmissionMode::Control,
            Requester::Input(key),
            None,
            command,
            now,
            deadline_ns,
        ) {
            Ok(admitted) => Ok(Ok(admitted)),
            Err(PreparationError::Rejected(e)) => Ok(Err(e)),
            Err(PreparationError::Fatal(e)) => {
                self.poison();
                Err(e)
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        requester: Requester,
        call: Option<Call>,
        command: Command,
        now: u64,
        deadline_ns: u64,
    ) -> Result<(u32, Vec<Plan>), PreparationError> {
        let deadline = now
            .checked_add(deadline_ns)
            .ok_or_else(|| OperationError::invalid("effect deadline overflow"))?;
        let (facts, effect, handle, healing) = match command {
            Command::Spawn { machine } => {
                adapter.reserve_spawn(&machine)?;
                (
                    Facts {
                        name: machine,
                        boot: 0,
                        vm: Vm::Up,
                        process: Process::Running,
                        physically_ended: false,
                        ready: false,
                        execution: ExecutionConfig::default(),
                    },
                    Effect::Spawn,
                    None,
                    false,
                )
            }
            Command::Heal { handle: text } => {
                let plan = self
                    .handles
                    .heal(&text)
                    .map_err(|e| error(ErrorCode::InvalidState, format!("heal: {e:?}")))?;
                let facts = match adapter.resolve(&Target::Key(plan.target.clone())) {
                    Ok(f) => f,
                    Err(e) => {
                        self.handles
                            .rejected(&plan)
                            .expect("matching local reservation");
                        return Err(e.into());
                    }
                };
                (facts, Effect::Execution(plan.config), Some(plan), true)
            }
            Command::Throttle { target, config } => {
                let facts = adapter.resolve(&target)?;
                let plan = self
                    .handles
                    .throttle(facts.name.clone(), config)
                    .map_err(|e| error(ErrorCode::InvalidState, format!("throttle: {e:?}")))?;
                (facts, Effect::Execution(config), Some(plan), false)
            }
            Command::Effect { target, effect } => (adapter.resolve(&target)?, effect, None, false),
        };
        self.install(
            adapter, mode, requester, call, facts, effect, handle, healing, now, deadline,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn install(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        requester: Requester,
        call: Option<Call>,
        facts: Facts,
        effect: Effect,
        handle: Option<ExecutionPlan>,
        healing: bool,
        now: u64,
        deadline: u64,
    ) -> Result<(u32, Vec<Plan>), PreparationError> {
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
            Effect::Execution(_) | Effect::Spawn => true,
        };
        if !allowed {
            return Err(error(ErrorCode::InvalidState, "target state rejects operation").into());
        }
        if matches!(mode, AdmissionMode::Trigger) != matches!(requester, Requester::Trigger(_)) {
            return Err(Failure::from("admission mode does not match its requester").into());
        }
        let fault = self.token()?;
        let pending = Pending {
            call,
            requester,
            target: facts.name.clone(),
            boot: facts.boot,
            effect: effect.clone(),
            command: None,
            dispatched: None,
            deadline,
            admitted: now,
            result: match &handle {
                _ if effect == Effect::Spawn => json!({"machine":facts.name}),
                Some(h) if !healing => json!({"handle":h.handle.to_string()}),
                _ => json!({}),
            },
            handle,
            healing,
        };
        let unavailable = matches!(pending.effect, Effect::Process(_))
            && (facts.vm == Vm::Down || facts.physically_ended);
        self.notices.push(Notice::Admitted(Admitted {
            fault,
            requester: pending.requester.clone(),
            target: pending.target.clone(),
            boot: pending.boot,
            action: Self::action(&pending),
            handle: Self::handle(&pending),
            requested_at: Self::requested_at(&pending),
            admitted_at: now,
        }));
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
        if pending.effect == Effect::Spawn {
            return Ok(Some(Ok(())));
        }
        let facts = match adapter.resolve(&Target::Key(pending.target.clone())) {
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
                Effect::Pause | Effect::Resume | Effect::Shutdown | Effect::Process(_)
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
            Effect::Execution(_) | Effect::Spawn => true,
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
        self.notices.push(Notice::Settled(settlement.clone()));
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
            requester: p.requester.clone(),
            target: p.target.clone(),
            boot: p.boot,
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
    fn action(p: &Pending) -> FaultAction {
        if p.healing {
            FaultAction::Heal
        } else {
            FaultAction::Effect(p.effect.clone())
        }
    }
    fn requested_at(p: &Pending) -> u64 {
        p.call.as_ref().map_or(p.admitted, |c| c.requested)
    }
    fn handle(p: &Pending) -> Option<String> {
        p.handle.as_ref().map(|h| h.handle.to_string())
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
            // The resolution causes any fault admission; publish it first.
            let mark = self.notices.len();
            let resolved = |status, fault| Notice::TriggerResolved {
                trigger: effect.trigger_id,
                at,
                status,
                fault,
            };
            let result = self.prepare(
                adapter,
                AdmissionMode::Trigger,
                Requester::Trigger(effect.trigger_id),
                None,
                Command::Effect {
                    target: Target::Key(effect.target),
                    effect: Effect::Process(ApplicationCommand::Stop),
                },
                at,
                PROCESS_EFFECT_TIMEOUT_NS,
            );
            match result {
                Ok((id, p)) => {
                    self.triggers.resolve(effect.trigger_id, at, Ok(id))?;
                    self.notices
                        .insert(mark, resolved(super::triggers::Status::Fired, Some(id)));
                    plans.extend(p);
                }
                Err(PreparationError::Rejected(e)) => {
                    self.triggers
                        .resolve(effect.trigger_id, at, Err(e.message))?;
                    self.notices
                        .insert(mark, resolved(super::triggers::Status::Rejected, None));
                }
                Err(PreparationError::Fatal(mut e)) => {
                    // A preparation failure after policy admission can already
                    // carry the new fault's Failed settlement. Fired records
                    // admission, not a successful guest effect.
                    if let Some(settlement) = e.settlements.first() {
                        let fault = settlement.fault;
                        self.triggers.resolve(effect.trigger_id, at, Ok(fault))?;
                        self.notices
                            .insert(mark, resolved(super::triggers::Status::Fired, Some(fault)));
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
        for trigger in self.triggers.expire(at) {
            self.notices.push(Notice::TriggerResolved {
                trigger,
                at,
                status: super::triggers::Status::Expired,
                fault: None,
            });
        }
    }
    pub(crate) fn cut_off_triggers(&mut self, at: u64) {
        for trigger in self.triggers.cut_off(at) {
            self.notices.push(Notice::TriggerResolved {
                trigger,
                at,
                status: super::triggers::Status::CutOff,
                fault: None,
            });
        }
    }
    pub(crate) fn trigger_boundary(&self) -> Option<u64> {
        self.triggers.boundary()
    }
}
#[cfg(test)]
mod tests;
