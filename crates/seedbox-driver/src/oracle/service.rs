//! Private control policy. Plans are executed by the owner of the single Driver
//! timeline; returning a plan never confirms its effect. Facts, command IDs and
//! first terminal-transition times must come from that owner, including changes
//! made outside this service. Machine snapshots alone do not snapshot this state.
use super::{
    handles::{Change, ExecutionPlan, HandleTarget, Handles},
    triggers::{TriggerAction, Triggers},
    *,
};
use crate::{
    clock::ClockRate,
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
/// of physical state. Crash/Reboot validate retained lifecycle independently;
/// a confirmed physical crash (`ExitReason::Crashed`) makes that lifecycle Down.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CommandOwner {
    External,
    RetiredBoot,
    RetiredApplication,
    Service { fault: u32, action: CommandKind },
}
/// A correlated guest command: an application operation or a realtime step.
/// Both share one per-boot ID space and acknowledge through CommandAcknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandKind {
    Application(ApplicationCommand),
    StepRealtime,
}
pub(crate) trait Adapter {
    /// Draw 64 seeded bits from `target`'s clock-fault stream at admission.
    fn draw(&mut self, _target: &str) -> Result<u64, OperationError> {
        Err(OperationError::unsupported())
    }
    /// Reserve a declared machine once, without building it or drawing entropy.
    fn reserve_spawn(&mut self, _: &str) -> Result<(), OperationError> {
        Err(OperationError::unsupported())
    }
    fn resolve(&self, target: &Target) -> Result<Facts, OperationError>;
    /// Check indirect heal targets too. SDK adapters retain their unrestricted
    /// authority; the Driver's guest view can exclude its controller.
    fn check_control_target(&self, _target: &Target) -> Result<(), OperationError> {
        Ok(())
    }
    /// Canonical name of an admitted machine with a network interface, in any
    /// lifecycle state. A declared but unspawned machine is TargetUnavailable;
    /// an unknown or unattached machine is InvalidRequest.
    fn network_member(&self, _target: &Target) -> Result<String, OperationError> {
        Err(OperationError::unsupported())
    }
    /// Reserve from the shared boot allocator, excluding even future scheduled
    /// external IDs. Reservation is not machine acceptance: command_owner must
    /// reject a receipt until the owner actually admits this plan.
    fn network_floor(&self) -> Result<u64, OperationError> {
        Err(OperationError::unsupported())
    }
    fn connections(
        &self,
        _from: &str,
        _to: &str,
        _after: u64,
    ) -> Result<Vec<Connection>, OperationError> {
        Err(OperationError::unsupported())
    }
    fn selection_available(&self, _connection: &Connection) -> Result<bool, OperationError> {
        Err(OperationError::unsupported())
    }
    fn selection_counts(
        &self,
        _handle: &str,
    ) -> Result<Option<crate::driver::NetworkSettlement>, OperationError> {
        Err(OperationError::unsupported())
    }
    fn command_id(
        &mut self,
        target: &str,
        boot: u64,
        fault: u32,
        action: CommandKind,
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
    /// Driver-applied rate of the target's time base.
    Drift(ClockRate),
    /// Guest-performed realtime step; zero settles without a guest command.
    StepRealtime(i64),
    Process(ApplicationCommand),
    /// Cut the fault's links (Driver-applied network state).
    Partition,
    /// Restore the links a partition handle cut; never a guest effect.
    Unpartition,
    Shape(crate::driver::NetworkPolicy, NetworkFaults),
    Unshape,
    Select,
    Unselect,
}
/// A validated admission subject before its fault ID is allocated.
enum Subject {
    Network(Scope),
    Machine(Facts),
    Links {
        left: Vec<String>,
        right: Vec<String>,
    },
}
/// What a fault acts on: one machine boot, or links between machines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    DirectedLinks {
        from: Vec<String>,
        to: Vec<String>,
    },
    Connection {
        connection: Connection,
    },
    Machine {
        name: String,
        boot: u64,
    },
    /// Both directions of every left × right pair; bound to no boot.
    Links {
        left: Vec<String>,
        right: Vec<String>,
    },
}
impl Scope {
    pub(crate) fn machine(&self) -> Option<(&str, u64)> {
        match self {
            Self::Machine { name, boot } => Some((name, *boot)),
            Self::Links { .. } | Self::DirectedLinks { .. } | Self::Connection { .. } => None,
        }
    }
    fn is_machine(&self, target: &str, boot: u64) -> bool {
        self.machine() == Some((target, boot))
    }
}
impl Effect {
    /// Effects carried out by the guest and completed by its acknowledgment.
    pub(crate) fn guest_command(&self) -> bool {
        self.command_kind().is_some()
    }
    fn command_kind(&self) -> Option<CommandKind> {
        match *self {
            Self::Process(action) => Some(CommandKind::Application(action)),
            Self::StepRealtime(_) => Some(CommandKind::StepRealtime),
            _ => None,
        }
    }
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
    Named {
        handle: String,
        command: Box<Command>,
    },
    HealRecord {
        record_id: super::handles::RecordId,
    },
    Shape {
        from: Vec<Target>,
        to: Vec<Target>,
        policy: crate::driver::NetworkPolicy,
        network_faults: NetworkFaults,
    },
    DropConnection {
        connection: Connection,
    },
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
    /// Step the target's guest realtime (`clock_skew`).
    StepRealtime {
        target: Target,
        nanoseconds: Amount,
    },
    /// Set the rate of the target's time base in ppb (`clock_drift`).
    Drift {
        target: Target,
        ppb: Amount,
    },
    /// Cut both directions between every left and right member (`partition`).
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
}
/// A clock fault value: exact, or drawn uniformly from [-max, max] by the
/// simulation RNG at admission.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Amount {
    Exact(i64),
    Seeded(u64),
}
impl Command {
    /// Exactly one of the exact or seeded value; the schema has bounded both.
    fn amount(exact: Option<i64>, max: Option<u64>) -> Result<Amount, OperationError> {
        match (exact, max) {
            (Some(exact), None) => Ok(Amount::Exact(exact)),
            (None, Some(max)) => Ok(Amount::Seeded(max)),
            _ => Err(OperationError::invalid("clock fault needs one value")),
        }
    }
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
            Operation::ClockSkew {
                target,
                offset_ns,
                max_offset_ns,
            } => Ok(Self::StepRealtime {
                target,
                nanoseconds: Self::amount(offset_ns, max_offset_ns)?,
            }),
            Operation::ClockDrift {
                target,
                rate_ppb,
                max_rate_ppb,
            } => Ok(Self::Drift {
                target,
                ppb: Self::amount(rate_ppb, max_rate_ppb)?,
            }),
            Operation::Partition { left, right } => Ok(Self::Partition { left, right }),
            Operation::Shape {
                from,
                to,
                latency_ns,
                jitter_ns,
                loss_ppm,
                bandwidth_bps,
                queue_bound_ns,
                network_faults,
            } => Ok(Self::Shape {
                from,
                to,
                policy: crate::driver::NetworkPolicy {
                    latency_ns,
                    jitter_ns,
                    loss_ppm,
                    bandwidth_bps,
                    queue_bound_ns,
                },
                network_faults,
            }),
            Operation::DropConnection { connection } => Ok(Self::DropConnection { connection }),
            _ => Err(OperationError::unsupported()),
        }
    }
}
/// Policy admission of a fault. Admission is not completion; a later
/// Settlement reports the outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Admitted {
    pub record_id: Option<super::handles::RecordId>,
    pub fault: u32,
    pub requester: Requester,
    pub scope: Scope,
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
        /// The Stop target; None for a partition, named by its admission.
        target: Option<String>,
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
    pub record_id: Option<super::handles::RecordId>,
    pub network: Option<crate::driver::NetworkSettlement>,
    pub fault: u32,
    pub requester: Requester,
    pub scope: Scope,
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
                    Some(*s)
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

type HandleAdmission = (Subject, Effect, Option<ExecutionPlan>, bool);
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
    // Immediate receipts never enter the timeline; box only this private arm
    // instead of inflating every queued control/input with optional counts.
    Settled(Box<Settlement>),
    Effect {
        fault: u32,
        scope: Scope,
        at: u64,
        effect: Effect,
        command: Option<NonZeroU32>,
        /// The handle a partition installs or a heal restores.
        handle: Option<String>,
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
        boot: Option<u64>,
        source: String,
        id: SimulationControlId,
        at: u64,
        payload: Vec<u8>,
    },
}
#[derive(Clone)]
struct Call {
    boot: Option<u64>,
    source: String,
    id: SimulationControlId,
    requested: u64,
    admitted: u64,
}
#[derive(Clone)]
struct Pending {
    network: Option<crate::driver::NetworkSettlement>,
    call: Option<Call>,
    requester: Requester,
    scope: Scope,
    effect: Effect,
    command: Option<NonZeroU32>,
    dispatched: Option<u64>,
    deadline: u64,
    admitted: u64,
    handle: Option<ExecutionPlan>,
    result: Value,
    healing: bool,
    /// Settlement detail of an effect applied without a guest command.
    note: Option<&'static str>,
}
#[derive(Clone, Default)]
pub(crate) struct Service {
    pub(crate) reply_latency_ns: u64,
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
            record_id: p.handle.as_ref().map(|h| h.record_id),
            fault,
            requester: p.requester.clone(),
            scope: p.scope.clone(),
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
        &self,
        call: Call,
        now: u64,
        effect: Option<u64>,
        result: Result<Value, OperationError>,
    ) -> Result<Plan, Failure> {
        if now < call.admitted {
            return Err("reply precedes admission".into());
        }
        let reply_base = if self.reply_latency_ns == 0 {
            now
        } else {
            effect.unwrap_or(now)
        };
        let replied = reply_base
            .checked_add(self.reply_latency_ns)
            .ok_or("control reply time overflow")?;
        let payload = encode(
            call.id.request,
            ReplyMetadata {
                requested_at_ns: call.requested,
                admitted_at_ns: call.admitted,
                effect_at_ns: effect,
                replied_at_ns: replied,
            },
            result.as_ref().map_err(Clone::clone),
        )
        .map_err(|e| format!("reply encoding: {e:?}"))?;
        Ok(Plan::Reply {
            boot: call.boot,
            source: call.source,
            id: call.id,
            at: replied,
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
            Ok(Operation::Connections { from, to, after }) => (|| {
                let from = adapter.network_member(&from)?;
                let to = adapter.network_member(&to)?;
                if from == to {
                    return Err(OperationError::invalid("connection endpoints must differ"));
                }
                let connections = adapter.connections(&from, &to, after)?;
                let next = connections.last().map_or(after, |c| c.incarnation);
                Ok(json!({"connections":connections,"next_after":next}))
            })(),
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
                            TriggerAction::Stop(target.name.clone()),
                        )
                        .map_err(|e| error(ErrorCode::InvalidState, e))?;
                    let deadline = self.triggers.records()[id as usize - 1].match_deadline;
                    self.notices.push(Notice::TriggerArmed {
                        trigger: id,
                        source: source.name,
                        target: Some(target.name),
                        at: now,
                        deadline,
                    });
                    Ok(json!({"trigger_id":id}))
                })
            }),
            Ok(Operation::ArmTrigger {
                source,
                marker,
                match_timeout_ns,
                fault: TriggerFault::Partition { left, right },
            }) => adapter.resolve(&source).and_then(|source| {
                // The same membership rules as an untriggered partition.
                let (left, right) = Self::partition_sides(adapter, &left, &right)?;
                let id = self
                    .triggers
                    .arm(
                        source.name.clone(),
                        marker,
                        now,
                        match_timeout_ns,
                        TriggerAction::Partition { left, right },
                    )
                    .map_err(|e| error(ErrorCode::InvalidState, e))?;
                let deadline = self.triggers.records()[id as usize - 1].match_deadline;
                self.notices.push(Notice::TriggerArmed {
                    trigger: id,
                    source: source.name,
                    target: None,
                    at: now,
                    deadline,
                });
                Ok(json!({"trigger_id":id}))
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
                | Operation::ClockSkew { .. }
                | Operation::ClockDrift { .. }
                | Operation::Spawn { .. }
                | Operation::Partition { .. }
                | Operation::Shape { .. }
                | Operation::DropConnection { .. }),
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
        };
        let effect = (result.is_ok()
            || result
                .as_ref()
                .is_err_and(|e| e.code == ErrorCode::TargetUnavailable))
        .then_some(now);
        Ok(vec![self.reply(call, now, effect, result)?])
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
        let (name, command) = match command {
            Command::Named { handle, command } => {
                self.handles.check_name(&handle).map_err(|e| {
                    error(
                        ErrorCode::InvalidState,
                        if e == super::handles::Error::DuplicateActive {
                            "duplicate active handle".into()
                        } else {
                            format!("handle: {e:?}")
                        },
                    )
                })?;
                (Some(handle), *command)
            }
            command => (None, command),
        };
        let deadline = now
            .checked_add(deadline_ns)
            .ok_or_else(|| OperationError::invalid("effect deadline overflow"))?;
        let (subject, effect, handle, healing) = match command {
            Command::Spawn { machine } => {
                adapter.reserve_spawn(&machine)?;
                (
                    Subject::Machine(Facts {
                        name: machine,
                        boot: 0,
                        vm: Vm::Up,
                        process: Process::Running,
                        physically_ended: false,
                        ready: false,
                        execution: ExecutionConfig::default(),
                    }),
                    Effect::Spawn,
                    None,
                    false,
                )
            }
            Command::Named { .. } => {
                return Err(OperationError::invalid("nested named admission").into())
            }
            Command::Heal { handle: text } => {
                let plan = self.handles.heal(&text);
                self.prepare_heal(adapter, mode, plan)?
            }
            Command::HealRecord { record_id } => {
                if !self.handles.active_record(record_id) {
                    return Err(
                        error(ErrorCode::InvalidState, "input owns no active handle").into(),
                    );
                }
                let plan = self.handles.heal_record(record_id);
                self.prepare_heal(adapter, mode, plan)?
            }
            Command::Shape {
                from,
                to,
                policy,
                network_faults,
            } => {
                policy
                    .validate_shape(adapter.network_floor()?, &network_faults)
                    .map_err(OperationError::invalid)?;
                let (from, to) = Self::partition_sides(adapter, &from, &to)?;
                let plan = self.reserve_handle(
                    HandleTarget::DirectedLinks {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    Change::Shape(policy, network_faults),
                    name.clone(),
                )?;
                (
                    Subject::Network(Scope::DirectedLinks { from, to }),
                    Effect::Shape(policy, network_faults),
                    Some(plan),
                    false,
                )
            }
            Command::DropConnection { connection } => {
                Operation::DropConnection {
                    connection: connection.clone(),
                }
                .validate()?;
                let from = adapter.network_member(&Target::Machine(connection.from.clone()))?;
                let to = adapter.network_member(&Target::Machine(connection.to.clone()))?;
                if from == to {
                    return Err(OperationError::invalid("connection endpoints must differ").into());
                }
                let plan = self.reserve_handle(
                    HandleTarget::Connection(connection.clone()),
                    Change::Select,
                    name.clone(),
                )?;
                let available = adapter.selection_available(&connection);
                if !matches!(available, Ok(true)) {
                    self.handles
                        .rejected(&plan)
                        .map_err(|_| Failure::from("connection reservation rollback"))?;
                    return Err(available
                        .err()
                        .unwrap_or_else(|| {
                            error(
                                ErrorCode::InvalidState,
                                "connection incarnation unavailable or selector capacity exhausted",
                            )
                        })
                        .into());
                }
                (
                    Subject::Network(Scope::Connection { connection }),
                    Effect::Select,
                    Some(plan),
                    false,
                )
            }
            Command::Throttle { target, config } => {
                let facts = adapter.resolve(&target)?;
                let plan = self.reserve_handle(
                    HandleTarget::Machine(facts.name.clone()),
                    Change::Execution(config),
                    name.clone(),
                )?;
                (
                    Subject::Machine(facts),
                    Effect::Execution(config),
                    Some(plan),
                    false,
                )
            }
            Command::StepRealtime {
                target,
                nanoseconds,
            } => {
                let facts = adapter.resolve(&target)?;
                Self::step_state(mode, &facts)?;
                // Draw only after admission checks, at this barrier.
                let offset = match nanoseconds {
                    Amount::Exact(offset) => offset,
                    Amount::Seeded(max) => Self::uniform(adapter, &facts.name, max)?,
                };
                let plan = self.reserve_handle(
                    HandleTarget::Machine(facts.name.clone()),
                    Change::Step(offset),
                    name.clone(),
                )?;
                (
                    Subject::Machine(facts),
                    Effect::StepRealtime(offset),
                    Some(plan),
                    false,
                )
            }
            Command::Drift { target, ppb } => {
                let facts = adapter.resolve(&target)?;
                // A machine runs at one rate. Like a refused throttle, a
                // refused duplicate consumes its handle number; it draws no value.
                if self.handles.drifting(&facts.name) {
                    if name.is_none() {
                        self.handles.reserve().map_err(|e| {
                            error(ErrorCode::InvalidState, format!("clock_drift: {e:?}"))
                        })?;
                    }
                    return Err(error(
                        ErrorCode::InvalidState,
                        "target machine clock is already drifting",
                    )
                    .into());
                }
                let ppb = match ppb {
                    Amount::Exact(rate) => rate,
                    Amount::Seeded(max) => Self::uniform(adapter, &facts.name, max)?,
                };
                let rate =
                    ClockRate::new(ppb).map_err(|e| OperationError::invalid(e.to_string()))?;
                let plan = self.reserve_handle(
                    HandleTarget::Machine(facts.name.clone()),
                    Change::Drift(rate),
                    name.clone(),
                )?;
                (
                    Subject::Machine(facts),
                    Effect::Drift(rate),
                    Some(plan),
                    false,
                )
            }
            Command::Partition { left, right } => {
                let (left, right) = Self::partition_sides(adapter, &left, &right)?;
                let plan = self.reserve_handle(
                    HandleTarget::Links {
                        left: left.clone(),
                        right: right.clone(),
                    },
                    Change::Partition,
                    name.clone(),
                )?;
                (
                    Subject::Links { left, right },
                    Effect::Partition,
                    Some(plan),
                    false,
                )
            }
            Command::Effect { target, effect } => (
                Subject::Machine(adapter.resolve(&target)?),
                effect,
                None,
                false,
            ),
        };
        self.install(
            adapter, mode, requester, call, subject, effect, handle, healing, now, deadline,
        )
    }
    fn reserve_handle(
        &mut self,
        target: HandleTarget,
        change: Change,
        name: Option<String>,
    ) -> Result<ExecutionPlan, OperationError> {
        let label = match change {
            Change::Execution(_) => "throttle",
            Change::Drift(_) => "clock_drift",
            Change::Step(_) => "clock_skew",
            Change::Partition => "partition",
            Change::Shape(..) => "shape",
            Change::Select => "drop_connection",
            _ => "handle",
        };
        let result = if let Some(name) = name {
            self.handles.named(target, change, name)
        } else {
            match (target, change) {
                (HandleTarget::Machine(name), Change::Execution(c)) => {
                    self.handles.throttle(name, c)
                }
                (HandleTarget::Machine(name), Change::Drift(c)) => self.handles.drift(name, c),
                (HandleTarget::Machine(name), Change::Step(n)) => self.handles.step(name, n),
                (HandleTarget::Links { left, right }, Change::Partition) => {
                    self.handles.partition(left, right)
                }
                (target, change) => self.handles.network(target, change),
            }
        };
        result.map_err(|e| error(ErrorCode::InvalidState, format!("{label}: {e:?}")))
    }
    fn prepare_heal(
        &mut self,
        adapter: &impl Adapter,
        mode: AdmissionMode,
        plan: Result<ExecutionPlan, super::handles::Error>,
    ) -> Result<HandleAdmission, PreparationError> {
        let plan = plan.map_err(|e| error(ErrorCode::InvalidState, format!("heal: {e:?}")))?;
        let subject = match Self::heal_subject(adapter, mode, &plan) {
            Ok(s) => s,
            Err(e) => {
                self.handles
                    .rejected(&plan)
                    .expect("matching local reservation");
                return Err(e.into());
            }
        };
        let effect = match plan.change {
            Change::Execution(c) => Effect::Execution(c),
            Change::Drift(c) => Effect::Drift(c),
            Change::Step(n) => Effect::StepRealtime(n),
            Change::Unpartition => Effect::Unpartition,
            Change::Unshape => Effect::Unshape,
            Change::Unselect => Effect::Unselect,
            _ => return Err(Failure::from("heal reapplies installed policy").into()),
        };
        Ok((subject, effect, Some(plan), true))
    }
    pub(crate) fn input_record_active(&self, id: super::handles::RecordId) -> bool {
        self.handles.active_record(id)
    }
    pub(crate) fn handle_collision(&self, fault: u32) -> bool {
        self.pending
            .get(&fault)
            .and_then(|p| p.handle.as_ref())
            .is_some_and(|p| self.handles.collision(p))
    }
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

    /// The subject of a heal. A partition heal restores links and binds to no
    /// machine boot; undoing a live step needs the same guest as the step.
    fn heal_subject(
        adapter: &impl Adapter,
        mode: AdmissionMode,
        plan: &ExecutionPlan,
    ) -> Result<Subject, OperationError> {
        let name = match &plan.target {
            HandleTarget::Links { left, right } => {
                for name in left.iter().chain(right) {
                    adapter.check_control_target(&Target::Key(name.clone()))?;
                }
                return Ok(Subject::Links {
                    left: left.clone(),
                    right: right.clone(),
                });
            }
            HandleTarget::DirectedLinks { from, to } => {
                for name in from.iter().chain(to) {
                    adapter.check_control_target(&Target::Key(name.clone()))?;
                }
                return Ok(Subject::Network(Scope::DirectedLinks {
                    from: from.clone(),
                    to: to.clone(),
                }));
            }
            HandleTarget::Connection(connection) => {
                adapter.check_control_target(&Target::Machine(connection.from.clone()))?;
                adapter.check_control_target(&Target::Machine(connection.to.clone()))?;
                return Ok(Subject::Network(Scope::Connection {
                    connection: connection.clone(),
                }));
            }
            HandleTarget::Machine(name) => name,
        };
        let facts = adapter.resolve(&Target::Key(name.clone()))?;
        if matches!(plan.change, Change::Step(_)) && !plan.lost {
            Self::step_state(mode, &facts)?;
        }
        Ok(Subject::Machine(facts))
    }
    /// Resolve partition sides to sorted canonical member names before any
    /// handle allocation: nonempty, attached, unique and disjoint.
    fn partition_sides(
        adapter: &impl Adapter,
        left: &[Target],
        right: &[Target],
    ) -> Result<(Vec<String>, Vec<String>), OperationError> {
        if left.is_empty() || right.is_empty() {
            return Err(OperationError::invalid("partition sides must be nonempty"));
        }
        let resolve = |side: &[Target]| {
            side.iter()
                .map(|target| adapter.network_member(target))
                .collect::<Result<Vec<_>, _>>()
        };
        let (mut left, mut right) = (resolve(left)?, resolve(right)?);
        let mut all: Vec<&String> = left.iter().chain(&right).collect();
        all.sort();
        if all.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(OperationError::invalid(
                "partition sides must be disjoint with unique machines",
            ));
        }
        left.sort();
        right.sort();
        Ok((left, right))
    }
    /// Realtime steps are guest commands and share the process-command state
    /// rules, applied before any seeded draw.
    fn step_state(mode: AdmissionMode, facts: &Facts) -> Result<(), OperationError> {
        if matches!(mode, AdmissionMode::Control) && facts.physically_ended && facts.vm != Vm::Down
        {
            return Err(error(ErrorCode::TargetUnavailable, "target VM has exited"));
        }
        // As for process commands: paused is refused; a down target is
        // admitted and then settles Unavailable (see `install`).
        match facts.vm {
            Vm::Paused => Err(error(ErrorCode::InvalidState, "target machine is paused")),
            Vm::Up | Vm::Down => Ok(()),
        }
    }
    /// Uniform in [-max, max] by rejection sampling of seeded 64-bit draws.
    fn uniform(adapter: &mut impl Adapter, target: &str, max: u64) -> Result<i64, OperationError> {
        let span = max
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .filter(|_| i64::try_from(max).is_ok())
            .ok_or_else(|| OperationError::invalid("seeded clock bound overflow"))?;
        let limit = u64::MAX - u64::MAX % span;
        loop {
            let draw = adapter.draw(target)?;
            if draw < limit {
                return Ok((draw % span) as i64 - max as i64);
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn install(
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
            Effect::Execution(_) | Effect::Spawn | Effect::Drift(_) | Effect::StepRealtime(_) => {
                true
            }
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
            result: match &handle {
                _ if effect == Effect::Spawn => json!({"machine":facts.name}),
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
            },
            // These static notes form a closed wire list in persistence.rs.
            // Update that list when adding or changing a note here.
            note: match &handle {
                Some(h) if h.lost => Some("realtime step ended with its machine incarnation"),
                _ if effect == Effect::StepRealtime(0) => Some("zero realtime step"),
                _ => None,
            },
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
        if !unavailable && (!self.pending[&fault].effect.guest_command() || facts.ready) {
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
        let mut plans = vec![];
        for (&fault, p) in &mut self.pending {
            if p.scope.is_machine(target, boot)
                && p.effect.guest_command()
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
            Effect::Execution(_) | Effect::Spawn | Effect::Drift(_) => true,
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
    fn finish(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
        result: Result<(), OperationError>,
    ) -> Result<Vec<Plan>, Failure> {
        self.finish_inner(fault, observed, at, result, None)
    }
    fn finish_inner(
        &mut self,
        fault: u32,
        observed: u64,
        at: u64,
        result: Result<(), OperationError>,
        forced: Option<SettlementStatus>,
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
        self.reply(call, at, Some(at), Ok(json!({})))
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
                match effect.action {
                    TriggerAction::Stop(target) => Command::Effect {
                        target: Target::Key(target),
                        effect: Effect::Process(ApplicationCommand::Stop),
                    },
                    // Members are re-resolved by the identical admission rules.
                    TriggerAction::Partition { left, right } => Command::Partition {
                        left: left.into_iter().map(Target::Key).collect(),
                        right: right.into_iter().map(Target::Key).collect(),
                    },
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

mod persist;
